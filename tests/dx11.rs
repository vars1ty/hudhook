mod harness;
mod hook;

use std::thread;
use std::time::Duration;

use harness::dx11::Dx11Harness;
use hook::HookExample;
use hudhook::hooks::dx11::ImguiDx11Hooks;
use hudhook::*;
use tracing::metadata::LevelFilter;

#[test]
fn test_imgui_dx11() {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::TRACE)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_names(true)
        .init();

    let dx11_harness = Dx11Harness::new("DX11 hook example");
    thread::sleep(Duration::from_millis(500));

    if let Err(e) =
        Hudhook::builder().with(HookExample::new().into_hook::<ImguiDx11Hooks>()).build().apply()
    {
        eprintln!("Couldn't apply hooks: {e:?}");
    }

    thread::sleep(Duration::from_millis(5000));
    drop(dx11_harness);
}
