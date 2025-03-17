# callme

Audio calls with Iroh!

`callme` is an experimental library and tool that uses [iroh-roq](https://github.com/dignifiedquire/iroh-roq) to transfer Opus-encoded audio between devices. It uses [cpal](https://github.com/RustAudio/cpal) for cross-platform access to the device's audio interfaces. It includes optional audio processing with echo cancellation, and should run on most platforms.

## Crates

See the READMEs of the individual crates for usage instructions.

* **[callme](callme)** is the main Rust library used by all other crates in the workspace.
* **[callme-cli](callme-cli)** is a basic command-line tool to make audio calls.
* **[callme-egui](callme-egui)** is a GUI for callme. It runs on desktop (Linux, macOS, WindowS) and Android. iOS support is currently untested, but should work. See the [README](callme-egui/README.md) for detailed instructions.

## License

Copyright 2024 N0, INC.

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

