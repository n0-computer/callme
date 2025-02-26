# callme

one-to-one audio calls using iroh and [iroh-roq](https://github.com/dignifiedquire/iroh-roq)

** beware: very work-in-progress experiment **

## crates

* **[callme](callme)** is a Rust library. It uses [cpal](https://github.com/RustAudio/cpal) to record and play audio, encodes it to opus, and transfers it with `iroh-roq`.
* **[callme-cli](callme-cli)** is a basic command-line tool for one-to-one audio calls.
* **[callme-egui](callme-egui)** is a (very WIP) GUI. It runs on desktop, and android! iOS maybe too. See the [README](callme-egui/README.md) for how to run it on your phone.
* **[callme-web](callme-web)** runs callme on the web. However, it doesn't work, because [cpal doesn't support audio input in the browser, only output](https://github.com/RustAudio/cpal/issues/813)
