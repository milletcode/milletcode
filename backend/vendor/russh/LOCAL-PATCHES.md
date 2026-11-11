# Local patches to russh 0.48.2

This is an unmodified copy of [russh](https://github.com/Eugeny/russh) 0.48.2
(Apache-2.0, see [LICENSE](LICENSE)) plus one local patch, applied via
`[patch.crates-io]` in the workspace `Cargo.toml`.

## rsa-sha2-256/512 publickey auth (`src/client/encrypted.rs`)

Upstream russh/ssh-key always encode RSA keys under the `ssh-rsa` algorithm
tag, which modern OpenSSH servers disable. The patch adds
`auth_public_key_blob()`: when offering a public key whose algorithm is
`rsa-sha2-*`, it emits the algorithm name plus the standard RSA blob
(`string(algorithm) + mpint(e) + mpint(n)`) as raw bytes, avoiding an extra
length-prefix that `ssh-key`'s `KeyData::Other` encoding would add. This
makes RSA publickey authentication work against current OpenSSH defaults.

Rebase notes: if upstream gains an equivalent fix (or a newer release changes
`encrypted.rs`), drop the patch and return to the crates.io dependency.
