# libghostty-vt

Safe Rust API over `libghostty-vt-sys`.

Handle types (`Terminal`, `RenderState`, `KeyEncoder`, etc.) are `!Send + !Sync` by design. Callers should drive all operations from a single thread.
