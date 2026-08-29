#![no_std]

extern crate alloc;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

use asr::{
    future::next_tick,
    game_engine::unity::mono::Module,
    settings::Gui,
    Process,
};

asr::async_main!(stable);
asr::panic_handler!();

/// Splits for the Wonder category. Mirrors the split set of the ASL script that
/// this auto splitter replaces, so existing .lss files stay compatible.
#[derive(Gui)]
struct Settings {
    /// Forester
    #[default = true]
    forester: bool,
    /// Gear Workshop
    #[default = true]
    gear_workshop: bool,
    /// Tapper's Shack
    #[default = true]
    tappers_shack: bool,
    /// Observatory
    #[default = true]
    observatory: bool,
    /// Smelter + Wood Workshop
    #[default = true]
    smelter_woodworkshop: bool,
    /// Research Earth Recultivator
    #[default = true]
    research_earth_recultivator: bool,
    /// Earth Recultivator (Launch)
    #[default = true]
    earth_recultivator: bool,
}

async fn main() {
    let mut settings = Settings::register();

    asr::print_message("Timberborn auto splitter loaded.");

    loop {
        // On Windows the process is "Timberborn.exe"; the Linux/Proton build
        // reports "Timberborn.x86_64".
        let process = Process::wait_attach("Timberborn.exe").await;
        process
            .until_closes(async {
                let module = Module::wait_attach_auto_detect(&process).await;
                asr::print_message("Attached to the Mono runtime.");

                // Timberborn has no static roots into its DI container, so
                // services are located by scanning the heap for the single
                // instance of each service class. See docs/DESIGN.md.
                //
                // TODO: locate services, then poll them here.
                let _ = &module;

                loop {
                    settings.update();
                    next_tick().await;
                }
            })
            .await;
    }
}
