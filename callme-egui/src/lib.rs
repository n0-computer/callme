#[cfg(target_os = "android")]
use egui_winit::winit;

pub mod app;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use eframe::{NativeOptions, Renderer};
    use tracing_subscriber::{layer::SubscriberExt, EnvFilter};

    std::env::set_var("RUST_BACKTRACE", "full");
    std::env::set_var("RUST_LOG", "debug");

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("warn,callme=trace"))
        .pretty()
        .finish();
    let subscriber = {
        let android_layer = tracing_android::layer("callme").unwrap();
        subscriber.with(android_layer)
    };

    tracing::subscriber::set_global_default(subscriber).expect("Unable to set global subscriber");

    let options = NativeOptions {
        android_app: Some(app),
        renderer: Renderer::Wgpu,
        ..Default::default()
    };
    self::app::App::run(options).unwrap();
}
