# git-shim

`git-shim` builds a `git` executable that sits before the real Git binary on
`PATH`. It forwards normal Git commands unchanged. For `git commit`, it removes
only this trailer line from the final commit message:

```text
Co-Authored-By: Claude Opus 5.5 (1M context) noreply@anthropic.com
```

It also removes the usual angle-bracket email form and accepts any casing of
the `Co-Authored-By` key. Other message content and co-author trailers remain.

## Build and use

```sh
cargo build --release
export PATH="$(pwd)/target/release:$PATH"
git status
```

The shim finds the first executable named `git` later on `PATH`, skipping its
own binary, and runs that as the real Git executable. It makes no system-wide
Git configuration changes.

For a commit, the shim creates a temporary `core.hooksPath` overlay. It runs
every existing executable hook through that overlay, then runs its cleanup as
the final step of `commit-msg`. This covers messages supplied with `-m`, `-F`,
and an editor, while preserving existing hook failures.

## `--no-verify`

Git bypasses `commit-msg` hooks for `git commit --no-verify`. Because the shim
uses that hook to operate after Git finishes the message, `--no-verify` also
bypasses trailer removal.
