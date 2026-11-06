//! Keeping several requests outstanding on one file: [`ReadAhead`] and [`WriteBehind`]
//! (docs/map/territory/file-transfer.md).

use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroUsize};

use crate::client::{status_ok, unexpected, Download, Walk};
use crate::error::{Error, Result, StatusCode};
use crate::protocol::{Handle, OpenFlags, Request, Response};
use crate::session::{Sent, Session};

/// One outstanding `SSH_FXP_READ`.
struct PendingRead {
    offset: u64,
    len: u32,
    sent: Sent,
}

/// A file open for reading with up to `depth` requests outstanding, handing over its bytes **in
/// file order**.
///
/// Replies that arrive early wait in memory: `depth` chunks from a server that answers what was
/// asked, and at most `depth` times [`crate::Config::max_inbound_packet`] from one that does not,
/// since a reply's length is checked when it reaches the front. The first [`Self::next`] sends
/// `depth` requests whatever the file's size, and each later call sends what the last one took;
/// between calls nothing is sent. Every request gets its own [`crate::Config::request_timeout`] from
/// when it reaches the front of the queue.
///
/// How much depth helps depends on the SSH channel window under this crate, which it cannot see:
/// past what the window holds, more depth moves no more bytes and only waits longer.
///
/// **Other requests on the same [`Session`] wait behind what is outstanding.** Against OpenSSH's
/// server that is about `depth` chunks at the link's rate, so a slow link can make them report
/// [`Error::Timeout`] or [`Error::WriteTimeout`] with nothing wrong. Where they must stay quick, lower
/// the depth. A second [`Session`] over another SFTP channel of the same SSH connection stops the
/// wait growing with depth, but what the SSH channel window already has on its way still goes
/// first, so on a very slow link it helps less than a low depth does.
///
/// ⚠️ **[`Self::close`] must be called**, for the same reason as on [`crate::ReadFile`].
pub struct ReadAhead<'a> {
    session: &'a Session,
    handle: Handle,
    chunk_len: NonZeroU32,
    depth: usize,
    offset: u64,
    queue: VecDeque<PendingRead>,
    closed: bool,
}

impl ReadAhead<'_> {
    /// The next chunk of up to the chunk length, or `None` at end of file. A short chunk is not the
    /// end, and a zero-length reply is passed on as an empty one. Dropping this future loses nothing;
    /// the next call picks up the same chunk.
    ///
    /// A failure is reported after every byte before it. Calling again asks again from
    /// [`Self::offset`], as [`crate::ReadFile::next`] does. A reply longer than was asked for is
    /// refused with [`Error::TooLong`].
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        self.fill()?;
        let Some(front) = self.queue.front_mut() else {
            return Err(Error::Eof);
        };
        let answer = self.session.reply_to(&mut front.sent).await;
        let Some(front) = self.queue.pop_front() else {
            return Err(Error::Eof);
        };

        let data = match answer {
            Ok(Response::Data(data)) => data,
            Ok(Response::Status(s)) if s.code == StatusCode::Eof => {
                self.queue.clear();
                return Ok(None);
            }
            other => {
                self.queue.clear();
                return Err(unexpected("DATA", other));
            }
        };
        if data.len() as u64 > u64::from(front.len) {
            self.queue.clear();
            return Err(Error::TooLong {
                len: data.len() as u64,
                limit: u64::from(front.len),
            });
        }

        self.offset += data.len() as u64;
        // `data.len()` is at most `front.len`, a `u32`.
        let got = data.len() as u32;
        if got < front.len && !self.ask_for_the_rest(front.offset + u64::from(got), front.len - got)
        {
            self.queue.clear();
        }
        Ok(Some(data))
    }

    /// Where the next chunk starts: everything before it has been handed over.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Hands the handle back to the server. **Must be called, and must run to its end**: dropping
    /// this future before the replies still due have landed sends no `CLOSE`. Sends no further
    /// `READ`, and waits for those replies, each on its own clock from the front of the queue,
    /// discarding them.
    pub async fn close(mut self) -> Result<()> {
        while let Some(front) = self.queue.front_mut() {
            if self.session.reply_to(&mut front.sent).await.is_err() {
                self.queue.clear();
                break;
            }
            self.queue.pop_front();
        }
        self.closed = true;
        self.session.close_handle(&self.handle).await
    }

    /// Sends requests until `depth` are outstanding. A send that fails with others still outstanding
    /// is left for a later call, so what they carry is still delivered.
    fn fill(&mut self) -> Result<()> {
        let mut at = self
            .queue
            .back()
            .map_or(self.offset, |last| last.offset + u64::from(last.len));
        while self.queue.len() < self.depth {
            match self.send(at, self.chunk_len.get()) {
                Ok(pending) => self.queue.push_back(pending),
                Err(_) if !self.queue.is_empty() => break,
                Err(e) => return Err(e),
            }
            at += u64::from(self.chunk_len.get());
        }
        Ok(())
    }

    /// Puts the remainder of a short reply at the front. `false` if it could not be sent.
    fn ask_for_the_rest(&mut self, offset: u64, len: u32) -> bool {
        match self.send(offset, len) {
            Ok(pending) => {
                self.queue.push_front(pending);
                true
            }
            Err(_) => false,
        }
    }

    fn send(&self, offset: u64, len: u32) -> Result<PendingRead> {
        let sent = self.session.enqueue(Request::Read {
            handle: self.handle.clone(),
            offset,
            len,
        })?;
        Ok(PendingRead { offset, len, sent })
    }
}

impl Drop for ReadAhead<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.closed || std::thread::panicking(),
            "a ReadAhead was dropped without close() — the server handle is leaked. `close_handle` is async and Drop cannot await."
        );
    }
}

impl Session {
    /// Opens a file for reading with up to `depth` requests of `chunk_len` bytes outstanding. See
    /// [`ReadAhead`].
    pub async fn read_file_ahead(
        &self,
        path: &[u8],
        chunk_len: NonZeroU32,
        depth: NonZeroUsize,
    ) -> Result<ReadAhead<'_>> {
        self.read_file_ahead_from(path, 0, chunk_len, depth).await
    }

    /// [`Self::read_file_ahead`], starting at `offset`, for resuming a transfer.
    pub async fn read_file_ahead_from(
        &self,
        path: &[u8],
        offset: u64,
        chunk_len: NonZeroU32,
        depth: NonZeroUsize,
    ) -> Result<ReadAhead<'_>> {
        let handle = self.open_file(path, OpenFlags::READ).await?;
        Ok(ReadAhead {
            session: self,
            handle,
            chunk_len,
            depth: depth.get(),
            offset,
            queue: VecDeque::new(),
            closed: false,
        })
    }

    /// [`Self::read_file_watched`] with up to `depth` requests outstanding. `on_chunk` is told in file
    /// order. Answering [`Walk::Stop`] sends no further request and closes the handle; the bytes
    /// already on their way are waited for and discarded, so a stop can take up to `depth` replies.
    pub async fn read_file_ahead_watched(
        &self,
        path: &[u8],
        chunk_len: NonZeroU32,
        depth: NonZeroUsize,
        on_chunk: &mut (dyn FnMut(&[u8], u64) -> Walk + Send),
    ) -> Result<Download> {
        let mut open = self.read_file_ahead(path, chunk_len, depth).await?;
        let mut read = 0u64;
        let mut stopped = false;
        let walk = async {
            while let Some(chunk) = open.next().await? {
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

/// One outstanding `SSH_FXP_WRITE`.
struct PendingWrite {
    len: u64,
    sent: Sent,
}

/// A file open for writing with up to `depth` writes outstanding.
///
/// [`Self::write`] returns once the bytes are queued, so **it does not mean the server has them**;
/// [`Self::acknowledged`] says how far it does. Every write gets its own
/// [`crate::Config::write_timeout`] and [`crate::Config::request_timeout`] from when it reaches the
/// front of the queue. As on [`ReadAhead`], past what the SSH channel window holds, more depth moves
/// no more bytes.
///
/// **Other requests on the same [`Session`] wait behind what is outstanding.** Against OpenSSH's
/// server that is about `depth` chunks at the link's rate, so a slow link can make them report
/// [`Error::Timeout`] or [`Error::WriteTimeout`] with nothing wrong. Where they must stay quick, lower
/// the depth. A second [`Session`] over another SFTP channel of the same SSH connection stops the
/// wait growing with depth, but what the SSH channel window already has on its way still goes
/// first, so on a very slow link it helps less than a low depth does.
///
/// ⚠️ **After a failure the file can hold bytes past [`Self::acknowledged`], with a hole before
/// them**: one write can fail while the next is accepted. Resume from that offset, not from the
/// file's size.
///
/// ⚠️ **[`Self::close`] must be called**, for the same reason as on [`crate::WriteFile`].
pub struct WriteBehind<'a> {
    session: &'a Session,
    handle: Handle,
    depth: usize,
    offset: u64,
    acknowledged: u64,
    queue: VecDeque<PendingWrite>,
    failed: Option<Error>,
    closed: bool,
}

impl WriteBehind<'_> {
    /// Queues `data` at the current offset, first waiting for the oldest write if `depth` are
    /// outstanding. `data` may not exceed [`Self::max_chunk`]; a refusal sends nothing and does not end
    /// the file. A write that fails, or that gets no answer in time, does: every later call reports
    /// the same failure. Dropping this future before it returns leaves `data` unqueued.
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.check()?;
        self.settle_finished()?;
        while self.queue.len() >= self.depth {
            self.settle_oldest().await?;
        }
        let req = self
            .session
            .write_request(&self.handle, self.offset, data)?;
        let sent = self.session.enqueue(req)?;
        self.queue.push_back(PendingWrite {
            len: data.len() as u64,
            sent,
        });
        self.offset += data.len() as u64;
        Ok(())
    }

    /// Waits for every queued write. After `Ok`, [`Self::acknowledged`] equals [`Self::offset`].
    pub async fn flush(&mut self) -> Result<()> {
        self.check()?;
        while !self.queue.is_empty() {
            self.settle_oldest().await?;
        }
        Ok(())
    }

    /// The offset up to which **every** byte is acknowledged by the server, with no hole before it.
    /// This is where to resume after a failure.
    pub fn acknowledged(&self) -> u64 {
        self.acknowledged
    }

    /// Where the next [`Self::write`] lands.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The largest `data` [`Self::write`] will accept; see [`crate::WriteFile::max_chunk`].
    pub fn max_chunk(&self) -> usize {
        self.session.max_write_len(&self.handle)
    }

    /// Waits for every queued write and hands the handle back, whichever way that went. **Must be
    /// called, and must run to its end**: dropping this future before the writes settle sends no
    /// `CLOSE`. To learn the resume point of a failure, call [`Self::flush`] first: this consumes the
    /// value.
    pub async fn close(mut self) -> Result<()> {
        let flushed = self.flush().await;
        self.closed = true;
        let closed = self.session.close_handle(&self.handle).await;
        flushed?;
        closed
    }

    fn check(&self) -> Result<()> {
        match &self.failed {
            Some(e) => Err(e.repeated()),
            None => Ok(()),
        }
    }

    /// Takes the outcomes already known, oldest first, without waiting.
    fn settle_finished(&mut self) -> Result<()> {
        while let Some(front) = self.queue.front_mut() {
            let Some(answer) = front.sent.try_reply() else {
                return Ok(());
            };
            self.settle(answer)?;
        }
        Ok(())
    }

    /// Waits for the oldest write. Dropping this future leaves it queued.
    async fn settle_oldest(&mut self) -> Result<()> {
        let Some(front) = self.queue.front_mut() else {
            return Ok(());
        };
        let answer = self.session.reply_to(&mut front.sent).await;
        self.settle(answer)
    }

    /// Records the oldest write's outcome. A failure gives up every reply still due.
    fn settle(&mut self, answer: Result<Response>) -> Result<()> {
        let Some(front) = self.queue.pop_front() else {
            return Ok(());
        };
        match status_ok(answer) {
            Ok(()) => {
                self.acknowledged += front.len;
                Ok(())
            }
            Err(e) => {
                self.queue.clear();
                self.failed = Some(e.repeated());
                Err(e)
            }
        }
    }
}

impl Drop for WriteBehind<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.closed || std::thread::panicking(),
            "a WriteBehind was dropped without close() — the server handle is leaked. `close_handle` is async and Drop cannot await."
        );
    }
}

impl Session {
    /// Opens a file for writing, created and truncated, with up to `depth` writes outstanding. See
    /// [`WriteBehind`].
    pub async fn write_file_behind(
        &self,
        path: &[u8],
        depth: NonZeroUsize,
    ) -> Result<WriteBehind<'_>> {
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        let handle = self.open_file(path, flags).await?;
        Ok(self.write_behind(handle, 0, depth))
    }

    /// Opens a file for writing and continues from `offset`, keeping what is already there — the
    /// flags of [`Self::write_file_from`] and the same duty on the caller: the bytes before `offset`
    /// must be a correct prefix of the source.
    pub async fn write_file_behind_from(
        &self,
        path: &[u8],
        offset: u64,
        depth: NonZeroUsize,
    ) -> Result<WriteBehind<'_>> {
        let handle = self
            .open_file(path, OpenFlags::WRITE | OpenFlags::CREATE)
            .await?;
        Ok(self.write_behind(handle, offset, depth))
    }

    fn write_behind(&self, handle: Handle, offset: u64, depth: NonZeroUsize) -> WriteBehind<'_> {
        WriteBehind {
            session: self,
            handle,
            depth: depth.get(),
            offset,
            acknowledged: offset,
            queue: VecDeque::new(),
            failed: None,
            closed: false,
        }
    }
}
