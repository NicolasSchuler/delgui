# Example inputs

Small fixtures for trying the app out by hand.

| files | shows |
| --- | --- |
| `config_before.rs` `config_after.rs` | code: syntax highlighting and word-level diffs |
| `config_variant.rs` | a third panel, to try reference selection and tabs |
| `motivation_draft1.txt` `motivation_draft2.txt` | prose: word-level diffs, correctly left unhighlighted |

```sh
cargo run --release -- examples/config_before.rs examples/config_after.rs
```
