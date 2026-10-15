#![no_std]
#![no_main]

#[cfg(all(feature = "console-uart", feature = "console-usb-jtag"))]
compile_error!("select exactly one console feature");

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_println::println!("IDFR_NOSTD_BOOT smoke-v1");
    let mut tick = 0u32;
    loop {
        let started = esp_hal::time::Instant::now();
        while started.elapsed() < esp_hal::time::Duration::from_secs(1) {
            core::hint::spin_loop();
        }
        tick = tick.wrapping_add(1);
        esp_println::println!("IDFR_NOSTD_TICK {}", tick);
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("IDFR_NOSTD_PANIC {}", info);
    loop {
        core::hint::spin_loop();
    }
}
