# Path verbs

## What it is

The one-round-trip verbs that act on a path or a handle and answer a status, a handle, attributes or
a name: `open_file`, `close_handle`, `stat`, `lstat`, `fstat`, `set_stat`, `set_stat_handle`,
`remove`, `rename`, `mkdir`, `rmdir`, `read_link`; and the four `expect_*` helpers that check the
reply's shape.

## Governing decisions

**None.**

## Design model

- **`open_file` always writes the attributes block, even when empty**, because § 6.3 makes it a
  mandatory field of `SSH_FXP_OPEN`. This is also where upstream #57 lands — *some servers refuse
  `open` with `PermissionDenied` unless `FileAttributes` is sent, though `stat` on the same file
  works*. It is a workaround against a third-party server, and the pointer at `open_file` is there so
  it is not later mistaken for stale and deleted.
- **`lstat` is the one a listing wants**, because it is what makes a link report as a link; `stat`
  follows symlinks.
- **`set_stat` has no `set_metadata(path, previously_read_attributes)` form and cannot have one**
  ([file attributes](file-attributes.md)). An empty update is not sent.
- **`rename` sends `from` before `to`**, and a swap usually *succeeds* against a real server — which
  is why the test asserts the bytes.
- **Symlink creation is absent**; `read_link` returns the target as bytes ([packets](packets.md)).
- **The `expect_*` helpers turn a failing `STATUS` into `Error::Status`** ([errors](errors.md)).

## Code

- `src/client.rs` — `Session::open_file`, `Session::open_file_with`, `Session::close_handle`,
  `Session::stat`, `Session::lstat`, `Session::fstat`, `Session::set_stat`,
  `Session::set_stat_handle`, `Session::remove`, `Session::rename`, `Session::mkdir`,
  `Session::rmdir`, `Session::read_link`, `expect_ok`, `expect_handle`, `expect_attrs`,
  `expect_name`, `unexpected`

## Reference behaviour

- Upstream #57 is a row in [reference](../reference.md#russh-sftp).

## Cross-cutting invariants

- [Paths are bytes](../invariant/paths-are-bytes.md)

## Blast radius

- [File attributes](file-attributes.md), [packets](packets.md), [errors](errors.md).

## Known holes / open

**None.**
