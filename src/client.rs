//! The verb surface.
//!
//! Listing, transfer and basic metadata (`docs/map/territory/scope.md`). Full protocol coverage is
//! explicitly not the goal; the skeleton underneath it is.

use crate::attrs::{AttrsUpdate, FileAttributes};
use crate::error::{Error, Result, StatusCode};
use crate::protocol::{DirEntry, Handle, OpenFlags, Request, Response};
use crate::session::Session;

/// Whether a watched walk carries on after the batch it was just told about.
///
/// An enum rather than a `bool` because both answers are ordinary — a walk that runs to the end and
/// a walk the user stopped are equally correct outcomes — and `false` at a call site reads as a
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// Ask the server for the next batch.
    Continue,
    /// Do not ask again. The handle is still closed and what was read so far still comes back.
    Stop,
}

/// The result of a walk, and **whether it is the whole directory**.
///
/// ⚠️ The flag is load-bearing rather than informational: without it a caller cannot tell *"this
/// folder holds twenty things"* from *"I stopped after twenty"*, and drawing the second as the
/// first is a listing that lies about the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<DirEntry>,
    /// `true` when the caller answered [`Walk::Stop`] before the server reported `EOF`.
    pub stopped: bool,
}

/// The result of reading a file, and **whether it is the whole file**.
///
/// ⚠️ The flag carries the same weight `Listing::stopped` does, and one step more of it: a caller
/// that ignores it hands a **truncated** copy to whatever opens files on this machine. A short
/// listing draws as a folder with fewer rows; a short file opens as a config that stops in the
/// middle of a line, and nothing on screen says so.
///
/// There is no `Vec<u8>` here on purpose. A 4 GB download that had to be assembled in memory before
/// anyone could write it would be a size limit disguised as a return type — the bytes go out through
/// the callback, one chunk at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Download {
    /// How many bytes were handed to the callback.
    pub bytes: u64,
    /// `true` when the caller answered [`Walk::Stop`] before the server reported `EOF`.
    pub stopped: bool,
}

impl Session {
    /// Canonicalises a path. Also the way to learn the remote home directory: v3 servers resolve
    /// `.` to it.
    ///
    /// ⚠️ The NAME reply here carries **one entry whose attributes are a dummy** — the draft says so
    /// in § 6.11, and `russh-sftp` has a `FileAttributes::dummy()` constructor for exactly this.
    /// Only the filename is meaningful, which is why this returns bytes and not a `DirEntry`.
    pub async fn real_path(&self, path: &[u8]) -> Result<Vec<u8>> {
        let mut names = self.expect_name(Request::RealPath { path: path.to_vec() }).await?;
        if names.is_empty() {
            return Err(Error::UnexpectedReply { expected: "NAME with one entry", got: 104 });
        }
        Ok(names.remove(0).filename)
    }

    pub async fn open_dir(&self, path: &[u8]) -> Result<Handle> {
        self.expect_handle(Request::OpenDir { path: path.to_vec() }).await
    }

    /// One batch of directory entries. `Ok(None)` is the server saying there are no more.
    ///
    /// ⚠️ **A directory is read in batches and the count is the server's choice**, so a single call
    /// is not a listing. `Eof` arrives as a `STATUS`, not as an empty `NAME`, which is why the
    /// signature is an `Option` rather than a `Vec` that happens to be empty.
    pub async fn read_dir(&self, handle: &Handle) -> Result<Option<Vec<DirEntry>>> {
        match self.request(Request::ReadDir { handle: handle.clone() }).await {
            Ok(Response::Name(entries)) => Ok(Some(entries)),
            Ok(Response::Status(s)) if s.code == StatusCode::Eof => Ok(None),
            other => Err(unexpected("NAME", other)),
        }
    }

    /// Walks a directory to the end and closes it.
    ///
    /// `.` and `..` are **not** filtered here. They are real entries the server sent, and deciding
    /// whether to draw them is the consumer's — this crate addresses files, it does not present
    /// them.
    pub async fn list_dir(&self, path: &[u8]) -> Result<Vec<DirEntry>> {
        self.list_dir_watched(path, &mut |_, _| Walk::Continue).await.map(|l| l.entries)
    }

    /// The same walk, with the caller told after every batch and able to end it.
    ///
    /// `on_batch` is handed the batch that just landed and the **running total** read so far, and
    /// answers whether to keep going.
    /// One callback rather than two, because the two questions have the same answer moment: a
    /// batch has just landed, so this is both the only new thing to report and the only place the
    /// walk can be interrupted.
    ///
    /// # Why the loop is here and not in the caller
    ///
    /// The handle is closed whichever way the walk ends — see [`Session::list_dir`]'s note on
    /// finite server handles. A caller that drove `open_dir`/`read_dir` itself in order to insert
    /// a cancel check would have to reproduce that discipline, and a copy of an invariant is how
    /// two copies stop agreeing. So the walk stays here and the *decision* is injected.
    ///
    /// # Two costs, both inherent
    ///
    /// - ⚠️ **There is no total.** SFTP has no verb that answers how many entries a directory
    ///   holds; `READDIR` returns batches until `EOF`. A caller wanting *"340 of 1,284"* cannot
    ///   have it, and a progress bar with a denominator would have to invent one.
    /// - ⚠️ **A stop is felt at the next batch boundary, not immediately.** The check sits between
    ///   round trips because that is where the walk yields; on a slow link that is one round trip
    ///   of latency. Ending it sooner would mean dropping the future, which skips the close.
    ///
    /// Nothing here decides *how often* to report or *what makes* a stop — both are policy and
    /// belong to whoever calls this.
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

        // The handle is closed whichever way the walk ended — to the end, on a failure, **and on a
        // stop**. A server has a finite number of open handles and leaking one per listing exhausts
        // them, and a user who stops several slow listings would strand one each time.
        let closed = self.close_handle(&handle).await;
        walk?;
        closed?;
        Ok(Listing { entries: all, stopped })
    }

    /// Opens a file.
    ///
    /// ⚠️ **The attributes block is always written, even when empty**, because § 6.3 makes it a
    /// mandatory field of `SSH_FXP_OPEN`. This is also where upstream #57 lands — *some servers
    /// refuse `open` with `PermissionDenied` unless `FileAttributes` is sent, though `stat` on the
    /// same file works*. That is a workaround against a **third-party server**, which this project
    /// permits so long as it says why it exists; the note is here so it is not later mistaken for
    /// stale and deleted.
    pub async fn open_file(&self, path: &[u8], flags: OpenFlags) -> Result<Handle> {
        self.open_file_with(path, flags, AttrsUpdate::new()).await
    }

    pub async fn open_file_with(
        &self,
        path: &[u8],
        flags: OpenFlags,
        attrs: AttrsUpdate,
    ) -> Result<Handle> {
        self.expect_handle(Request::Open { path: path.to_vec(), flags, attrs }).await
    }

    /// Reads at an offset. `Ok(None)` is end of file.
    ///
    /// ⚠️ **A short read is normal and is not the end of the file.** The draft says so for device
    /// files in § 6.4 — *"this may return fewer bytes than requested"* — so a caller that treats
    /// `data.len() < len` as EOF truncates. Loop until this returns `None`.
    pub async fn read(&self, handle: &Handle, offset: u64, len: u32) -> Result<Option<Vec<u8>>> {
        match self.request(Request::Read { handle: handle.clone(), offset, len }).await {
            Ok(Response::Data(data)) => Ok(Some(data)),
            Ok(Response::Status(s)) if s.code == StatusCode::Eof => Ok(None),
            other => Err(unexpected("DATA", other)),
        }
    }

    /// Reads a file from the beginning to the end, with the caller told after every chunk and able
    /// to end it — and closes the handle whichever way it ended.
    ///
    /// `on_chunk` is handed the bytes that just arrived **and the running total**, and answers
    /// whether to keep going. One callback rather than two for the reason
    /// [`Self::list_dir_watched`] gives: a chunk has just landed, so this is both the only new thing
    /// to report and the only place the read can be interrupted.
    ///
    /// # Why the loop is here and not in the caller
    ///
    /// The same argument, unchanged: the handle is closed to the end, on a failure, **and on a
    /// stop**, and a caller that drove `open_file`/`read` itself in order to insert a cancel check
    /// would have to reproduce that discipline. A copy of an invariant is how two copies stop
    /// agreeing.
    ///
    /// # `chunk_len` is the caller's, and that is the other half of the split
    ///
    /// [`crate::Config::max_outbound_packet`]'s note gives splitting policy — how big, how many in
    /// flight, whether to re-issue a short read — to the caller. So this
    /// loop does not reach for [`Session::max_read_len`] on its own: it takes a length and sends it.
    /// The mechanism (the loop, the offset, the close) is the crate's; the policy is injected, the
    /// same way [`Walk`] injects the decision to stop.
    ///
    /// # What this deliberately does not do
    ///
    /// - **It writes nothing.** This crate has no filesystem concern; where the bytes land is the
    ///   app's, and a `Vec<u8>` return would put a memory ceiling on a 4 GB download.
    /// - **It does not `stat` first.** A denominator for a progress bar is the caller's question and
    ///   costs a round trip, so the caller pays it only if it wants one. Unlike a listing, it *can*
    ///   have one — `SSH_FXP_STAT` answers a file's size, and no verb answers a directory's length.
    /// - **A zero-length `DATA` reply is not treated as `EOF`**, and is not guarded against either.
    ///   The draft gives it no meaning, so inventing one here would be inventing a workaround for a
    ///   server defect nobody has measured. The failure it would produce is a total that stops
    ///   moving on screen while the caller's stop still works on the next chunk — visible and
    ///   reversible, which is this repository's bar for not writing a guard.
    ///
    /// ⚠️ **A stop is felt at the next chunk boundary**, not immediately — the check sits between
    /// round trips because that is where the read yields. Ending it sooner would mean dropping the
    /// future, which skips the close.
    /// Opens a file for reading, driven by the caller. See [`ReadFile`] — and prefer
    /// [`Self::read_file_watched`] unless you genuinely need to pull.
    pub async fn read_file(&self, path: &[u8]) -> Result<ReadFile<'_>> {
        self.read_file_from(path, 0).await
    }

    /// Opens a file for reading and **starts partway in**.
    ///
    /// The one caller is resume: a transfer that found a shorter file at the destination continues
    /// from its length rather than sending the whole source again. Nothing about the read changes —
    /// `SSH_FXP_READ` carries the offset on the wire, so this only decides where the first one
    /// points.
    ///
    /// ⚠️ **An offset past the end of the file is not an error here.** The server answers the first
    /// read with `EOF` and [`ReadFile::next`] reports `Ok(None)`, i.e. an empty file. Deciding
    /// whether that is sensible belongs to the caller, which is the only place that knows what the
    /// offset was supposed to mean.
    pub async fn read_file_from(&self, path: &[u8], offset: u64) -> Result<ReadFile<'_>> {
        let handle = self.open_file(path, OpenFlags::READ).await?;
        Ok(ReadFile { session: self, handle, offset, closed: false })
    }

    pub async fn read_file_watched(
        &self,
        path: &[u8],
        chunk_len: u32,
        on_chunk: &mut (dyn FnMut(&[u8], u64) -> Walk + Send),
    ) -> Result<Download> {
        // Built on [`ReadFile`] so there is **one** place a read handle is closed. The offset
        // arithmetic — advance by what arrived, never by what was asked for — lives there too.
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
        Ok(Download { bytes: read, stopped })
    }

}

/// A file open for reading on the server, **driven by the caller**.
///
/// # Why this exists next to `read_file_watched`
///
/// That function owns its loop, which is right when the destination is passive — a local file, a
/// progress bar. It cannot serve **remote to remote**, where the far side's
/// [`Session::write_file_watched`] wants to own a loop of its own and neither can yield. Pulling is
/// the shape that lets one drive the other.
///
/// # ⚠️ The close is a request here, not a structure
///
/// [`Session::close_handle`] is `async` and Rust has no async drop, so [`Drop`] **cannot** do the
/// work. A caller that forgets [`Self::close`] leaks a server handle, and a server has a finite
/// number of them.
///
/// What `Drop` can do is refuse to be quiet: it fires a `debug_assert!`, so forgetting is a failing
/// test rather than a server that stops opening files after a few hundred transfers. In release it
/// is a leak, which is what it would have been anyway.
///
/// **Prefer `read_file_watched` wherever the destination is passive.** This is for the case that
/// one cannot express.
pub struct ReadFile<'a> {
    session: &'a Session,
    handle: Handle,
    offset: u64,
    closed: bool,
}

impl ReadFile<'_> {
    /// The next chunk, or `None` at end of file.
    ///
    /// ⚠️ **A short answer is not the end.** Only `None` is (see [`Session::read`]). The offset is
    /// tracked here and advances by what arrived, never by what was asked for.
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

    /// Hands the handle back to the server. **Must be called** — see the type's note.
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        self.session.close_handle(&self.handle).await
    }
}

impl Drop for ReadFile<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.closed,
            "a ReadFile was dropped without close() — the server handle is leaked.              `close_handle` is async and Drop cannot await, so this assertion is the only thing              standing between a forgotten close and a server that quietly stops opening files."
        );
    }
}

/// A file open for writing on the server, **driven by the caller** — the mirror of [`ReadFile`].
///
/// # Why this exists next to `write_file_watched`
///
/// That function pulls, through a **synchronous** [`Feed`] callback, which is enough when the
/// source is a local file. It is not enough when the source is itself asynchronous — a
/// remote-to-remote copy has to `await` the far side's read inside the loop, and a sync callback
/// cannot. Pushing is the shape that lets an async source drive an async sink.
///
/// # ⚠️ The same accepted cost as `ReadFile`
///
/// The close is a request, not a structure: `close_handle` is async, Rust has no async drop, and a
/// forgotten [`Self::close`] leaks a server handle. `Drop` fires a `debug_assert!` so that
/// forgetting is a failing test rather than a server that quietly stops opening files.
pub struct WriteFile<'a> {
    session: &'a Session,
    handle: Handle,
    offset: u64,
    closed: bool,
}

impl WriteFile<'_> {
    /// Appends `data` at the current offset.
    ///
    /// ⚠️ **There is no short write** — `SSH_FXP_WRITE` answers `SSH_FXP_STATUS`, so it wrote
    /// everything or it failed. The offset advances by exactly what was handed over.
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.session.write(&self.handle, self.offset, data).await?;
        self.offset += data.len() as u64;
        Ok(())
    }

    /// How many bytes the server has acknowledged.
    pub fn written(&self) -> u64 {
        self.offset
    }

    /// The largest `data` [`Self::write`] will accept — the twin of [`Session::max_read_len`]. The
    /// smaller of what fits the outbound packet and the server's stated write bound, if any.
    ///
    /// A caller driving its own loop reads exactly this much from its source. Splitting is still the
    /// caller's (see [`crate::Config::max_outbound_packet`]); what was missing was any way to know
    /// **how much fits**, and one caller filled that gap with a constant.
    ///
    /// ⚠️ **The ceiling is the packet, not the payload, and it is per file.** A `SSH_FXP_WRITE`
    /// encodes as length prefix(4) + type(1) + id(4) + handle(4 + len) + offset(8) + data(4 + len);
    /// `protocol.rs`'s encoding test pins that shape. The handle is **chosen by the server** and may
    /// be up to 256 bytes, so this cannot live on [`Session`] as a constant the way `max_read_len`
    /// does — a session-wide number would be right for OpenSSH's 4-byte handle and wrong, by exactly
    /// the difference, for a server that hands out longer ones.
    ///
    /// ⚠️ **Off by one in the small direction is the silent half.** Too large is refused loudly by
    /// [`Session::write`]; too small merely wastes a fraction of every packet and nothing ever says
    /// so. `tests/upload.rs` pins the value from both sides for that reason.
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

    /// Hands the handle back. **Must be called** — see the type's note.
    pub async fn close(mut self) -> Result<()> {
        self.closed = true;
        self.session.close_handle(&self.handle).await
    }
}

impl Drop for WriteFile<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.closed,
            "a WriteFile was dropped without close() — the server handle is leaked.              `close_handle` is async and Drop cannot await, so this assertion is the only thing              standing between a forgotten close and a server that quietly stops opening files."
        );
    }
}

/// What the caller has for the next chunk of an upload.
///
/// Three answers rather than an `Option`, because **"no more bytes" and "stop, the user cancelled"
/// are different outcomes** and a caller must not have to encode one as the other. A transfer that
/// reports a cancelled upload as a completed file is the same class of lie as a listing that draws
/// a stopped walk as a whole directory.
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

/// The result of writing a file, and **whether it is the whole file**.
///
/// ⚠️ `stopped` matters more here than on a download. A short local copy can be deleted; bytes a
/// server has already accepted cannot be unwritten, so a stopped upload leaves a **partial file on
/// somebody else's machine**. A caller that resumes writes into exactly that, which is why the
/// count is reported rather than rounded away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upload {
    /// How many bytes the server acknowledged.
    pub bytes: u64,
    /// `true` when the caller answered [`Feed::Stop`] before saying [`Feed::Done`].
    pub stopped: bool,
}

impl Session {
    /// Writes a file from the beginning, asking the caller for each chunk — and closes the handle
    /// whichever way it ended.
    ///
    /// The mirror of [`Self::read_file_watched`], with the same reason for the loop being here: the
    /// close discipline is this crate's invariant and a caller inserting a cancel check would have
    /// to reproduce it.
    ///
    /// # There is no short write, and that is the asymmetry with reading
    ///
    /// ⚠️ `SSH_FXP_READ` may answer with fewer bytes than asked for, so a download has to be
    /// careful not to read that as the end of the file. `SSH_FXP_WRITE` answers `SSH_FXP_STATUS` —
    /// it wrote everything or it failed. So the offset advances by exactly what was handed over and
    /// there is no partial-progress case to get wrong.
    ///
    /// # The destination is created and truncated
    ///
    /// ⚠️ Opening without `TRUNCATE` leaves the tail of a longer previous file behind, which is a
    /// **corrupt destination that reports success** — the write returns `Ok` for every chunk and
    /// the file is simply too long. Nothing downstream can notice.
    ///
    /// # `chunk_len` is the caller's, like the download's
    ///
    /// It is passed to `next` rather than used to split, so the caller reads exactly what fits from
    /// its own source instead of handing over more and having this split it again. Splitting policy
    /// is the caller's ([`crate::Config::max_outbound_packet`]).
    ///
    /// ⚠️ A caller that answers `Feed::Bytes` with an empty vector forever will loop, and that is
    /// not guarded — the same call this crate makes for a zero-length `DATA` reply on the read
    /// side. The loop is bounded by round trips rather than CPU, the caller controls it, and
    /// inventing a limit here would be inventing a policy for a case nobody has measured.
    /// Opens a file for writing, driven by the caller. See [`WriteFile`] — and prefer
    /// [`Self::write_file_watched`] unless the source is itself asynchronous.
    ///
    /// Created and truncated, for the reason `write_file_watched` records: without `TRUNCATE` the
    /// tail of a longer previous file survives, and every write still answers `Ok`.
    pub async fn write_file(&self, path: &[u8]) -> Result<WriteFile<'_>> {
        let handle = self
            .open_file(
                path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .await?;
        Ok(WriteFile { session: self, handle, offset: 0, closed: false })
    }

    /// Opens an **existing** file for writing, truncated: `WRITE | TRUNCATE` without `CREATE`, so a
    /// missing file is refused (`NoSuchFile`) rather than made. The in-place save of a file the user
    /// is editing — the same inode, so owner and permissions stay.
    pub async fn overwrite_file(&self, path: &[u8]) -> Result<WriteFile<'_>> {
        let handle = self
            .open_file(path, OpenFlags::WRITE | OpenFlags::TRUNCATE)
            .await?;
        Ok(WriteFile { session: self, handle, offset: 0, closed: false })
    }

    /// Opens a file for writing and **continues from `offset`, keeping what is already there**.
    ///
    /// ⚠️ **`TRUNCATE` is deliberately absent, and that is the whole difference from
    /// [`Self::write_file`].** Truncating is right for an ordinary copy — that method's own comment
    /// says why: without it the tail of a longer previous file survives every write reporting `Ok`.
    /// It is exactly wrong for a resume, where the bytes already there are the point.
    ///
    /// ⚠️ **`APPEND` is not used either, and that is not an oversight.** Under `SSH_FXP_OPEN`'s
    /// `APPEND` the server ignores the offset in each write and puts the data at the current end,
    /// so a resume that is wrong about the offset would still land bytes somewhere and report
    /// success. Writing at an explicit offset means a wrong offset writes to the wrong place
    /// *visibly*, which is the failure a caller can notice.
    ///
    /// The caller owes the invariant this cannot check: **the bytes already at the destination are
    /// a correct prefix of the source.** Nothing on the wire can confirm that — see
    /// `explorer_transfer`'s `resume_verdict`, which is where the decision is made and argued.
    pub async fn write_file_from(&self, path: &[u8], offset: u64) -> Result<WriteFile<'_>> {
        let handle = self
            .open_file(path, OpenFlags::WRITE | OpenFlags::CREATE)
            .await?;
        Ok(WriteFile { session: self, handle, offset, closed: false })
    }

    pub async fn write_file_watched(
        &self,
        path: &[u8],
        chunk_len: u32,
        next: &mut (dyn FnMut(u32) -> Feed + Send),
    ) -> Result<Upload> {
        // Built on [`WriteFile`] so there is **one** place a write handle is closed and one place
        // the offset advances.
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
        Ok(Upload { bytes: written, stopped })
    }
}

impl Session {
    pub async fn write(&self, handle: &Handle, offset: u64, data: &[u8]) -> Result<()> {
        // The outbound packet ceiling is enforced where every request is encoded; this is the
        // server's own write bound, which only a write has.
        if let Some(limit) = self.server_write_len(handle.as_bytes().len()) {
            if data.len() > limit {
                return Err(Error::TooLong { len: data.len() as u64, limit: limit as u64 });
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
        self.expect_ok(Request::Close { handle: handle.clone() }).await
    }

    /// Follows symlinks.
    pub async fn stat(&self, path: &[u8]) -> Result<FileAttributes> {
        self.expect_attrs(Request::Stat { path: path.to_vec() }).await
    }

    /// Does **not** follow symlinks — this is the one a directory listing wants, because it is what
    /// makes a link report as a link.
    pub async fn lstat(&self, path: &[u8]) -> Result<FileAttributes> {
        self.expect_attrs(Request::LStat { path: path.to_vec() }).await
    }

    pub async fn fstat(&self, handle: &Handle) -> Result<FileAttributes> {
        self.expect_attrs(Request::FStat { handle: handle.clone() }).await
    }

    /// Changes attributes. An empty update is refused rather than sent.
    ///
    /// ⚠️ There is no `set_metadata(path, previously_read_attributes)` here and there cannot be:
    /// [`AttrsUpdate`] has no constructor from [`FileAttributes`]. That is the read-modify-write
    /// that truncates files through `russh-sftp` — see the type's own documentation.
    pub async fn set_stat(&self, path: &[u8], attrs: AttrsUpdate) -> Result<()> {
        if attrs.is_empty() {
            return Ok(());
        }
        self.expect_ok(Request::SetStat { path: path.to_vec(), attrs }).await
    }

    pub async fn set_stat_handle(&self, handle: &Handle, attrs: AttrsUpdate) -> Result<()> {
        if attrs.is_empty() {
            return Ok(());
        }
        self.expect_ok(Request::FSetStat { handle: handle.clone(), attrs }).await
    }

    pub async fn remove(&self, path: &[u8]) -> Result<()> {
        self.expect_ok(Request::Remove { path: path.to_vec() }).await
    }

    pub async fn rename(&self, from: &[u8], to: &[u8]) -> Result<()> {
        self.expect_ok(Request::Rename { from: from.to_vec(), to: to.to_vec() }).await
    }

    pub async fn mkdir(&self, path: &[u8]) -> Result<()> {
        self.expect_ok(Request::MkDir { path: path.to_vec(), attrs: AttrsUpdate::new() }).await
    }

    pub async fn rmdir(&self, path: &[u8]) -> Result<()> {
        self.expect_ok(Request::RmDir { path: path.to_vec() }).await
    }

    /// The target of a symlink, as bytes. Creating one is deliberately absent — see [`Request`].
    pub async fn read_link(&self, path: &[u8]) -> Result<Vec<u8>> {
        let mut names = self.expect_name(Request::ReadLink { path: path.to_vec() }).await?;
        if names.is_empty() {
            return Err(Error::UnexpectedReply { expected: "NAME with one entry", got: 104 });
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

/// Turns "the reply was not the kind this verb needed" into an error that names both sides.
///
/// A failing `STATUS` becomes [`Error::Status`] rather than a shape complaint, because *the server
/// said no* is the useful sentence — the caller wants `NoSuchFile`, not "expected HANDLE".
fn unexpected(expected: &'static str, got: Result<Response>) -> Error {
    match got {
        Ok(Response::Status(s)) => Error::Status(s),
        Ok(other) => Error::UnexpectedReply { expected, got: other.packet_type() },
        Err(e) => e,
    }
}
