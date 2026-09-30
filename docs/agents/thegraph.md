# thegraph

## What this project is

An SFTP package written in Rust.

## References

- draft-ietf-secsh-filexfer-02 — the SFTP v3 wire format this client speaks.
- OpenSSH `PROTOCOL` § 4.8 — `limits@openssh.com`.
- `russh-sftp` 2.4.0 and `openssh-sftp-protocol` — the two Rust implementations read while writing
  this one; what each gets wrong is in `docs/map/territory/upstream-traps.md`.
