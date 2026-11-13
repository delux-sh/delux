fn main() {
    std::process::exit(delux::run(std::env::args_os().skip(1).collect()));
}
