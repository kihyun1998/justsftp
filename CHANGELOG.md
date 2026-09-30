# Changelog

## 0.1.0

First release. An SFTP v3 client over any `AsyncRead + AsyncWrite + Unpin + Send` stream:

- every path, filename, handle and extension name is `Vec<u8>`, sent back unchanged;
- listing (`list_dir`, `list_dir_watched`), reading (`read_file_watched`, `ReadFile`), writing
  (`write_file_watched`, `WriteFile`, `overwrite_file`, `write_file_from`), and `stat`, `lstat`,
  `fstat`, `set_stat`, `remove`, `rename`, `mkdir`, `rmdir`, `read_link`, `real_path`;
- `limits@openssh.com` asked at open, lowering read and write lengths;
- separate write and reply timeouts, cancel-safe requests.
