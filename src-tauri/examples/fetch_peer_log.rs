// Fetch the log of the controller this machine is paired with, without the
// app window. Quit MyKVM first: this stands in for it on its port.
//
//   cargo run --example fetch_peer_log -- "<config dir>" "<folder for the log>"

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(config_dir), Some(receive_dir)) = (args.next(), args.next()) else {
        eprintln!("usage: fetch_peer_log <config dir> <folder for the log>");
        std::process::exit(2);
    };
    match mykvm_lib::fetch_controller_log(
        std::path::Path::new(&config_dir),
        std::path::Path::new(&receive_dir),
    ) {
        Ok(path) => println!("{}", path.display()),
        Err(error) => {
            eprintln!("fetch failed: {error}");
            std::process::exit(1);
        }
    }
}
