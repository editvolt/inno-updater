# inno-updater

Windows background-update helper, used by [EditVolt](https://editvolt.com) during an
in-place update. Inno Setup launches it to swap the installed executables once the running
application has exited.

## Relationship to upstream

A fork of **[microsoft/inno-updater](https://github.com/microsoft/inno-updater)** at version
**0.24.0**, MIT licensed. The upstream copyright notices and `LICENSE` are unchanged, and
the update logic is untouched.

## What differs

Upstream hardcodes its own product name in five Rust literals and five Windows resource
strings, because upstream *is* that product. Here the name is a **build parameter**, so a
rebranded build does not need a patched source tree:

| Variable | Default |
|---|---|
| `INNO_UPDATER_PRODUCT_NAME` | `Visual Studio Code` |
| `INNO_UPDATER_COMPANY_NAME` | `Microsoft Corporation` |

`build.rs` reads them, emits `INNO_UPDATER_PRODUCT_NAME` for `env!()`, and substitutes
`{{PRODUCT_NAME}}` / `{{COMPANY_NAME}}` in `resources/resources.rc.template`. Both default
to upstream's values, so a plain `cargo build` reproduces upstream behaviour exactly.

## Building

Inno Setup is 32-bit, so the helper must be `i686`:

```bash
rustup target add i686-pc-windows-msvc
INNO_UPDATER_PRODUCT_NAME="Your Product" \
  cargo build --release --target i686-pc-windows-msvc --bin inno_updater
```

Requires Windows with the MSVC toolchain. Note that **Smart App Control blocks this build**:
Cargo compiles unsigned build scripts and an Enforced policy refuses to execute them
(`os error 4551`). CI runners have no such policy.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) Microsoft Corporation.
