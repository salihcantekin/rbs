//! `rbs-vcam-setup.exe register|unregister <SID>`: the elevated step of the RBS
//! virtual camera (ADR-0070 decision 5). The app starts it with the `runas` verb.

fn main() {
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        std::process::exit(rbs_vcam_mf::setup::run(&args));
    }
    #[cfg(not(windows))]
    {
        eprintln!("rbs-vcam-setup runs on Windows only");
        std::process::exit(2);
    }
}
