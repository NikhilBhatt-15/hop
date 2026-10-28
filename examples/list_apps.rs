//! Prints the app list hop indexes and how long enumeration took.
fn main() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let t = std::time::Instant::now();
    let apps = hop::apps::enumerate();
    let took = t.elapsed();
    for a in &apps {
        println!("{:<40} {}", a.name, a.id);
    }
    eprintln!("{} apps in {:?}", apps.len(), took);
}
