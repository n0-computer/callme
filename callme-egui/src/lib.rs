#[cfg(target_os = "android")]
use egui_winit::winit;

pub mod app;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use eframe::{NativeOptions, Renderer};

    std::env::set_var("RUST_BACKTRACE", "1");
    std::env::set_var("RUST_LOG", "warn,callme=debug");

    tracing_subscriber::fmt::init();

    // this would setup a android logging contxt
    // however then we get duplicate logs in the default adb output
    // because that displays stdout/stderr already.
    // use tracing_subscriber::{layer::SubscriberExt, EnvFilter};
    // let subscriber = tracing_subscriber::fmt()
    //     .with_env_filter(EnvFilter::new("warn,callme=debug"))
    //     .pretty()
    //     .finish();
    // let subscriber = {
    //     let android_layer = tracing_android::layer("callme").unwrap();
    //     subscriber.with(android_layer)
    // };
    // tracing::subscriber::set_global_default(subscriber).expect("Unable to set global subscriber");

    let options = NativeOptions {
        android_app: Some(app),
        renderer: Renderer::Wgpu,
        ..Default::default()
    };
    self::app::App::run(options).unwrap();
}
