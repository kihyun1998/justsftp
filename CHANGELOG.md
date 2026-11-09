# Changelog

## Unreleased

- `ReadAhead` and `WriteBehind` keep up to a caller-chosen depth of requests outstanding on one file,
  through `read_file_ahead`, `read_file_ahead_from`, `read_file_ahead_watched`, `write_file_behind` and
  `write_file_behind_from`. Bytes are handed over in file order. `WriteBehind::close` returns `Closed`,
  whose `acknowledged` is the offset to resume from after a failure.
- A `STATUS` of `EOF` answering a write, or any verb other than a read or a walk, is now
  `Error::Status`, not success.
- A request racing the end of the session now reports the cause rather than timing out.
- Dropping a `ReadFile` or `WriteFile` without closing it, or the future of a watched loop or a directory
  walk, now sends `CLOSE`. It used to panic in a debug build and leak the server handle in a release
  build. `close` still reports the result; a drop cannot.
- An `OPEN` or `OPENDIR` whose caller has gone, by a dropped future, `Timeout` or `WriteTimeout`, now has
  its handle closed when the reply arrives. It used to stay open on the server until the session ended.
- `tokio` 1.22 or later is required.
- `tokio`'s `macros` feature is no longer enabled by this crate.

## 0.1.0

First release. An SFTP v3 client over any `AsyncRead + AsyncWrite + Unpin + Send` stream:

- every path, filename, handle and extension name is `Vec<u8>`, sent back unchanged;
- listing (`list_dir`, `list_dir_watched`), reading (`read_file_watched`, `ReadFile`), writing
  (`write_file_watched`, `WriteFile`, `overwrite_file`, `write_file_from`), and `stat`, `lstat`,
  `fstat`, `set_stat`, `remove`, `rename`, `mkdir`, `rmdir`, `read_link`, `real_path`;
- `limits@openssh.com` asked at open, lowering read and write lengths;
- separate write and reply timeouts, cancel-safe requests.
