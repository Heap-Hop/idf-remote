fn main() {
    esp_idf_sys::link_patches();
    println!("IDFR_STD_BOOT smoke-v1");
    let mut tick = 0u32;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        tick = tick.wrapping_add(1);
        println!("IDFR_STD_TICK {tick}");
    }
}
