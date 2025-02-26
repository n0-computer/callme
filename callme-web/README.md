# callme-web

Callme from the browser.

NOTES:
* Does not work because [cpal does yet not support audio input](https://github.com/RustAudio/cpal/issues/813)
* Needs a patch to `webrtc-util`, see the commented-out section at the bottom of the repo root's Cargo.toml. With the patch enabled, we can compile to Wasm.

```
npm run build
npm run serve
```
