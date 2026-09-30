//! The verbs: listing, transfer and basic metadata (docs/map/territory/listing.md,
//! docs/map/territory/file-transfer.md, docs/map/territory/path-verbs.md).

use crate::attrs::{AttrsUpdate, FileAttributes};
use crate::error::{Error, Result, StatusCode};
use crate::protocol::{DirEntry, Handle, OpenFlags, Request, Response};
use crate::session::Session;

/// Whether a watched walk or read carries on after the batch or chunk it was just told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// Ask the server for the next batch or chunk.
    Continue,
    /// Do not ask again. The handle is still closed and what was read so far is still reported.
    Stop,
}

/// The result of a walk, and **whether it is the whole directory**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<DirEntry>,
    /// `true` when the caller answered [`Walk::Stop`] before the server reported `EOF`.
    pub stopped: bool,
}

/// The result of reading a file, and **whether it is the whole file**. The bytes themselves went to
/// the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Download {
    /// How many bytes were handed to the callback.
    pub bytes: u64,
    /// `true` when the caller answered [`Walk::Stop`] before the server reported `EOF`.
    pub stopped: bool,
}

impl Session {
    /// Canonicalises a path, as bytes. Also the way to learn the remote home directory: v3 servers
    /// resolve `.` to it.
    pub async fn real_path(&self, path: &[u8]) -> Result<Vec<u8>> {
        let mut names = self
            .expect_name(Request::RealPath {
                path: path.to_vec(),
            })
            .await?;
        if names.is_empty() {
            return Err(Error::UnexpectedReply {
                expected: "NAME with one entry",
                got: 104,
            });
        }
        Ok(names.remove(0).filename)
    }

    pub async fn open_dir(&self, path: &[u8]) -> Result<Handle> {
        self.expect_handle(Request::OpenDir {
            path: path.to_vec(),
        })
        .await
    }

    /// One batch of directory entries, of the server's chosen size. `Ok(None)` is the server saying
    /// there are no more.
    pub async fn read_dir(&self, handle: &Handle) -> Result<Option<Vec<DirEntry>>> {
        match self
            .request(Request::ReadDir {
                handle: handle.clone(),
            })
            .await
        {
            Ok(Response::Name(entries)) => Ok(Some(entries)),
            Ok(Response::Status(s)) if s.code == StatusCode::Eof => Ok(None),
            other => Err(unexpected("NAME", other)),
        }
    }

    /// Walks a directory to the end and closes it. `.` and `..` are returned if the server sends
    /// them.
    pub async fn list_dir(&self, path: &[u8]) -> Result<Vec<DirEntry>> {
        self.list_dir_watched(path, &mut |_, _| Walk::Continue)
            .await
            .map(|l| l.entries)
    }

    /// The same walk, with the caller told after every batch and able to end it.
    ///
    /// `on_batch` is handed the batch that just landed and the **running total** read so far, and
    /// answers whether to keep going. It runs only when a batch lands, so a stop waits up to one
    /// round trip; answering [`Walk::Stop`] sends no further request. The handle is closed whichever
    /// way the walk ends — at `EOF`, on a failure, or on a stop. SFTP has no way to ask how many
    /// entries a directory holds, so there is no total to report against.
    pub async fn list_dir_watched(
        &self,
        path: &[u8],
        on_batch: &mut (dyn FnMut(&[DirEntry], usize) -> Walk + Send),
    ) -> Result<Listing> {
        let handle = self.open_dir(path).await?;
        let mut all = Vec::new();
        let mut stopped = false;
        let walk = async {
            while let Some(batch) = self.read_dir(&handle).await? {
                let from = all.len();
                all.extend(batch);
                if on_batch(&all[from..], all.len()) == Walk::Stop {
                    stopped = true;
                    break;
                }
            }
            Ok::<(), Error>(())
        }
        .await;

        // Closed whichever way the walk ended (docs/map/invariant/every-handle-is-closed.md).
        let closed = self.close_handle(&handle).await;
        walk?;
        closed?;
        Ok(Listing {
            entries: all,
            stopped,
        })
    }

    /// Opens a file. An empty attributes block is always sent.
    // The empty block is mandatory in § 6.3 and some servers refuse `open` without it — not stale:
    // docs/map/territory/path-verbs.md.
    pub async fn open_file(&self, path: &[u8], flags: OpenFlags) -> Result<Handle> {
        self.open_file_with(path, flags, AttrsUpdate::new()).await
    }

    pub async fn open_file_with(
        &self,
        path: &[u8],
        flags: OpenFlags,
        attrs: AttrsUpdate,
    ) -> Result<Handle> {
        self.expect_handle(Request::Open {
            path: path.to_vec(),
            flags,
            attrs,
        })
        .await
    }

    /// Reads at an offset. `Ok(None)` is end of file.
    ///
    /// ⚠️ **A short read is not the end of the file**: the server may return fewer bytes than asked
    /// for. Loop until this returns `None`.
    pub async fn read(&self, handle: &Handle, offset: u64, len: u32) -> Result<Option<Vec<u8>>> {
        match self
            .request(Request::Read {
                handle: handle.clone(),
                offset,
                len,
            })
            .await
        {
            Ok(Response::Data(data)) => Ok(Some(data)),
            Ok(Response::Status(s)) if s.code == StatusCode::Eof => Ok(None),
            other => Err(unexpected("DATA", other)),
        }
    }

    /// Opens a file for reading, driven by the caller. See [`ReadFile`] — and prefer
    /// [`Self::read_file_watched`] unless you genuinely need to pull.
    pub async fn read_file(&self, path: &[u8]) -> Result<ReadFile<'_>> {
        self.read_file_from(path, 0).await
    }

    /// Opens a file for reading and **starts at `offset`**, for resuming a transfer.
    ///
    /// An offset past the end of the file is not an error: the first [`ReadFile::next`] returns
    /// `Ok(None)`.
    pub async fn read_file_from(&self, path: &[u8], offset: u64) -> Result<ReadFile<'_>> {
        let handle = self.open_file(path, OpenFlags::READ).await?;
        Ok(ReadFile {
            session: self,
            handle,
            offset,
            closed: false,
        })
    }

    /// Reads a file from the beginning to the end in requests of `chunk_len` bytes, with the caller
    /// told after every chunk and able to end it — and closes the handle whichever way it ended.
    ///
    /// `on_chunk` is handed the bytes that just arrived **and the running total**, and answers
    /// whether to keep going. It runs only when a chunk lands, so a stop waits up to one round trip;
    /// answering [`Walk::Stop`] sends no further request. Nothing is
    /// written anywhere and nothing is kept: the bytes are the callback's. A short read is not
    /// treated as the end; only the server's `EOF` is. A zero-length `DATA` reply is passed on as
    /// an empty chunk.
    pub async fn read_file_watched(
        &self,
        path: &[u8],
        chunk_len: u32,
        on_chunk: &mut (dyn FnMut(&[u8], u64) -> Walk + Send),
    ) -> Result<Download> {
        // Built on `ReadFile`, so one place closes a read handle and advances its offset.
        let mut open = self.read_file(path).await?;
        let mut read = 0u64;
        let mut stopped = false;
        let walk = async {
            while let Some(chunk) = open.next(chunk_len).await? {
                read += chunk.len() as u64;
                if on_chunk(&chunk, read) == Walk::Stop {
                    stopped = true;
                    break;
                }
            }
            Ok::<(), Error>(())
        }
        .await;

        let closed = open.close().await;
        walk?;
        closed?;
        Ok(Download {
            bytes: read,
            stopped,
        })
    }
}

/// A file open for reading on the server, **driven by the caller** — for a destination that must
/// itself drive a loop, such as a remote-to-remote copy. Prefer
/// [`Session::read_file_watched`] wherever the destination is passive.
///
/// ⚠️ **[`Self::close`] must be called.** `Drop` cannot close an SFTP handle, so it only fires a
/// `debug_assert!`; in release a forgotten close leaks the server handle.
pub struct ReadFile<'a> {
    session: &'a Session,
    handle: Handle,
    offset: u64,
    closed: bool,
}

impl ReadFile<'_> {
    /// The next chunk of up to `len` bytes, or `None` at end of file. A short chunk is not the end.
    /// The offset advances by what arrived.
    pub async fn next(&mut self, len: u32) -> Result<Option<Vec<u8>>> {
        match self.session.read(&self.handle, self.offset, len).await? {
            Some(data) => {
                self.offset += data.len() as u64;
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }

    /// The server's stated read length ([`Session::server_read_len`]).
    pub fn server_read_len(&self) -> Option<u32> {
        self.session.server_read_len()
    }

    /// Hands the handle back to the server. **Must be called.**
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        self.session.close_handle(&self.handle).await
    }
}

impl Drop for ReadFile<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.closed,
            "a ReadFile was dropped without close() — the server handle is leaked. `close_handle` is async and Drop cannot await, so this assertion is the only thing standing between a forgotten close and a server that quietly stops opening files."
        );
    }
}

/// A file open for writing on the server, **driven by the caller** — for a source that must
/// `await` between chunks, such as a remote-to-remote copy. The mirror of [`ReadFile`]; prefer
/// [`Session::write_file_watched`] where a synchronous [`Feed`] will do.
///
/// ⚠️ **[`Self::close`] must be called**, for the same reason as on [`ReadFile`].
pub struct WriteFile<'a> {
    session: &'a Session,
    handle: Handle,
    offset: u64,
    closed: bool,
}

impl WriteFile<'_> {
    /// Writes `data` at the current offset: all of it, or an error. `data` may not exceed
    /// [`Self::max_chunk`].
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.session.write(&self.handle, self.offset, data).await?;
        self.offset += data.len() as u64;
        Ok(())
    }

    /// How many bytes the server has acknowledged.
    pub fn written(&self) -> u64 {
        self.offset
    }

    /// The largest `data` [`Self::write`] will accept — the twin of [`Session::max_read_len`]: the
    /// smaller of what fits the outbound packet with this file's handle and the server's stated
    /// write bound, if any. It depends on the handle's length, so it is per file.
    pub fn max_chunk(&self) -> usize {
        let handle_len = self.handle.as_bytes().len();
        let ours = self
            .session
            .config()
            .max_outbound_packet
            .saturating_sub(crate::session::WRITE_OVERHEAD + handle_len);
        match self.session.server_write_len(handle_len) {
            Some(theirs) => ours.min(theirs),
            None => ours,
        }
    }

    /// Hands the handle back. **Must be called.**
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        self.session.close_handle(&self.handle).await
    }
}

impl Drop for WriteFile<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.closed,
            "a WriteFile was dropped without close() — the server handle is leaked. `close_handle` is async and Drop cannot await, so this assertion is the only thing standing between a forgotten close and a server that quietly stops opening files."
        );
    }
}

/// What the caller has for the next chunk of an upload. "No more bytes" and "stop" are different
/// answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feed {
    /// Write these. May be shorter than the length asked for; only the caller knows what its
    /// source had.
    Bytes(Vec<u8>),
    /// The file is complete.
    Done,
    /// Stop. The user cancelled, or the source could not be read any further.
    Stop,
}

/// The result of writing a file, and **whether it is the whole file**. A stopped upload leaves the
/// first `bytes` bytes on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upload {
    /// How many bytes the server acknowledged.
    pub bytes: u64,
    /// `true` when the caller answered [`Feed::Stop`] before saying [`Feed::Done`].
    pub stopped: bool,
}

impl Session {
    /// Opens a file for writing, created and truncated, driven by the caller. See [`WriteFile`] —
    /// and prefer [`Self::write_file_watched`] unless the source is itself asynchronous.
    pub async fn write_file(&self, path: &[u8]) -> Result<WriteFile<'_>> {
        let handle = self
            .open_file(
                path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .await?;
        Ok(WriteFile {
            session: self,
            handle,
            offset: 0,
            closed: false,
        })
    }

    /// Opens an **existing** file for writing, truncated: `WRITE | TRUNCATE` without `CREATE`, so a
    /// missing file is refused (`NoSuchFile`) rather than made. The file keeps its inode, owner and
    /// permissions.
    pub async fn overwrite_file(&self, path: &[u8]) -> Result<WriteFile<'_>> {
        let handle = self
            .open_file(path, OpenFlags::WRITE | OpenFlags::TRUNCATE)
            .await?;
        Ok(WriteFile {
            session: self,
            handle,
            offset: 0,
            closed: false,
        })
    }

    /// Opens a file for writing and **continues from `offset`, keeping what is already there** —
    /// `WRITE | CREATE`, with neither `TRUNCATE` nor `APPEND`, for resuming a transfer.
    ///
    /// The caller must know that the bytes already at the destination are a correct prefix of the
    /// source; nothing on the wire can confirm it.
    pub async fn write_file_from(&self, path: &[u8], offset: u64) -> Result<WriteFile<'_>> {
        let handle = self
            .open_file(path, OpenFlags::WRITE | OpenFlags::CREATE)
            .await?;
        Ok(WriteFile {
            session: self,
            handle,
            offset,
            closed: false,
        })
    }

    /// Writes a file from the beginning, created and truncated, asking the caller for each chunk —
    /// and closes the handle whichever way it ended.
    ///
    /// `next` is asked for up to `chunk_len` bytes each time and answers with a [`Feed`]; the caller
    /// sizes `chunk_len`, for example by [`WriteFile::max_chunk`]. Each write is all or an error,
    /// so the count in [`Upload`] is exact. An endless run of empty `Feed::Bytes` loops.
    pub async fn write_file_watched(
        &self,
        path: &[u8],
        chunk_len: u32,
        next: &mut (dyn FnMut(u32) -> Feed + Send),
    ) -> Result<Upload> {
        // Built on `WriteFile`, so one place closes a write handle and advances its offset.
        let mut open = self.write_file(path).await?;
        let mut stopped = false;
        let walk = async {
            loop {
                match next(chunk_len) {
                    Feed::Done => break,
                    Feed::Stop => {
                        stopped = true;
                        break;
                    }
                    Feed::Bytes(data) => open.write(&data).await?,
                }
            }
            Ok::<(), Error>(())
        }
        .await;

        let written = open.written();
        let closed = open.close().await;
        walk?;
        closed?;
        Ok(Upload {
            bytes: written,
            stopped,
        })
    }
}

impl Session {
    pub async fn write(&self, handle: &Handle, offset: u64, data: &[u8]) -> Result<()> {
        // The server's own write bound; the outbound ceiling is checked in `enqueue`.
        if let Some(limit) = self.server_write_len(handle.as_bytes().len()) {
            if data.len() > limit {
                return Err(Error::TooLong {
                    len: data.len() as u64,
                    limit: limit as u64,
                });
            }
        }
        self.expect_ok(Request::Write {
            handle: handle.clone(),
            offset,
            data: data.to_vec(),
        })
        .await
    }

    pub async fn close_handle(&self, handle: &Handle) -> Result<()> {
        self.expect_ok(Request::Close {
            handle: handle.clone(),
        })
        .await
    }

    /// Follows symlinks.
    pub async fn stat(&self, path: &[u8]) -> Result<FileAttributes> {
        self.expect_attrs(Request::Stat {
            path: path.to_vec(),
        })
        .await
    }

    /// Does **not** follow symlinks, so a link reports as a link.
    pub async fn lstat(&self, path: &[u8]) -> Result<FileAttributes> {
        self.expect_attrs(Request::LStat {
            path: path.to_vec(),
        })
        .await
    }

    pub async fn fstat(&self, handle: &Handle) -> Result<FileAttributes> {
        self.expect_attrs(Request::FStat {
            handle: handle.clone(),
        })
        .await
    }

    /// Changes attributes. An empty update is not sent.
    pub async fn set_stat(&self, path: &[u8], attrs: AttrsUpdate) -> Result<()> {
        if attrs.is_empty() {
            return Ok(());
        }
        self.expect_ok(Request::SetStat {
            path: path.to_vec(),
            attrs,
        })
        .await
    }

    pub async fn set_stat_handle(&self, handle: &Handle, attrs: AttrsUpdate) -> Result<()> {
        if attrs.is_empty() {
            return Ok(());
        }
        self.expect_ok(Request::FSetStat {
            handle: handle.clone(),
            attrs,
        })
        .await
    }

    pub async fn remove(&self, path: &[u8]) -> Result<()> {
        self.expect_ok(Request::Remove {
            path: path.to_vec(),
        })
        .await
    }

    pub async fn rename(&self, from: &[u8], to: &[u8]) -> Result<()> {
        self.expect_ok(Request::Rename {
            from: from.to_vec(),
            to: to.to_vec(),
        })
        .await
    }

    pub async fn mkdir(&self, path: &[u8]) -> Result<()> {
        self.expect_ok(Request::MkDir {
            path: path.to_vec(),
            attrs: AttrsUpdate::new(),
        })
        .await
    }

    pub async fn rmdir(&self, path: &[u8]) -> Result<()> {
        self.expect_ok(Request::RmDir {
            path: path.to_vec(),
        })
        .await
    }

    /// The target of a symlink, as bytes.
    pub async fn read_link(&self, path: &[u8]) -> Result<Vec<u8>> {
        let mut names = self
            .expect_name(Request::ReadLink {
                path: path.to_vec(),
            })
            .await?;
        if names.is_empty() {
            return Err(Error::UnexpectedReply {
                expected: "NAME with one entry",
                got: 104,
            });
        }
        Ok(names.remove(0).filename)
    }

    async fn expect_ok(&self, req: Request) -> Result<()> {
        match self.request(req).await {
            Ok(Response::Status(s)) if !s.code.is_error() => Ok(()),
            Ok(Response::Status(s)) => Err(Error::Status(s)),
            other => Err(unexpected("STATUS", other)),
        }
    }

    async fn expect_handle(&self, req: Request) -> Result<Handle> {
        match self.request(req).await {
            Ok(Response::Handle(h)) => Ok(h),
            other => Err(unexpected("HANDLE", other)),
        }
    }

    async fn expect_attrs(&self, req: Request) -> Result<FileAttributes> {
        match self.request(req).await {
            Ok(Response::Attrs(a)) => Ok(a),
            other => Err(unexpected("ATTRS", other)),
        }
    }

    async fn expect_name(&self, req: Request) -> Result<Vec<DirEntry>> {
        match self.request(req).await {
            Ok(Response::Name(n)) => Ok(n),
            other => Err(unexpected("NAME", other)),
        }
    }
}

/// Turns "the reply was not the kind this verb needed" into an error; a `STATUS` becomes
/// [`Error::Status`] (docs/map/territory/errors.md).
fn unexpected(expected: &'static str, got: Result<Response>) -> Error {
    match got {
        Ok(Response::Status(s)) => Error::Status(s),
        Ok(other) => Error::UnexpectedReply {
            expected,
            got: other.packet_type(),
        },
        Err(e) => e,
    }
}
