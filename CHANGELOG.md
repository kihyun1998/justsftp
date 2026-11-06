# Changelog

## Unreleased

- `ReadAhead` and `WriteBehind` keep up to a caller-chosen depth of requests outstanding on one file,
  through `read_file_ahead`, `read_file_ahead_from`, `read_file_ahead_watched`, `write_file_behind` and
  `write_file_behind_from`. Bytes are handed over in file order, and `WriteBehind::acknowledged` is the
  offset to resume from after a failure.
- A `STATUS` of `EOF` answering a write, or any verb other than a read or a walk, is now
  `Error::Status`, not success.
- A request racing the end of the session now reports the cause rather than timing out.
- A panic while holding a `ReadFile` or `WriteFile` no longer aborts a debug build.
- `tokio`'s `macros` feature is no longer enabled by this crate.

## 0.1.0

First release. An SFTP v3 client over any `AsyncRead + AsyncWrite + Unpin + Send` stream:

- every path, filename, handle and extension name is `Vec<u8>`, sent back unchanged;
- listing (`list_dir`, `list_dir_watched`), reading (`read_file_watched`, `ReadFile`), writing
  (`write_file_watched`, `WriteFile`, `overwrite_file`, `write_file_from`), and `stat`, `lstat`,
  `fstat`, `set_stat`, `remove`, `rename`, `mkdir`, `rmdir`, `read_link`, `real_path`;
- `limits@openssh.com` asked at open, lowering read and write lengths;
- separate write and reply timeouts, cancel-safe requests.
