use toad_computer::{App, Config, boot, serve};

fn usage() -> &'static str {
    "Usage: toad-computer <boot|serve> [--addr ADDRESS] [--token TOKEN] [--home PATH] [--display DISPLAY] [--screen WIDTHxHEIGHT]\n\n  boot   start the display, the session bus and the desktop, then serve; the container entrypoint\n  serve  serve on a display that already exists"
}

enum Command {
    Boot,
    Serve,
}

fn parse() -> Result<(Command, Config), String> {
    let mut args = std::env::args().skip(1);
    let command = match args.next().as_deref() {
        Some("boot") => Command::Boot,
        Some("serve") => Command::Serve,
        Some("--help" | "-h") | None => return Err(usage().to_owned()),
        Some(command) => return Err(format!("unknown subcommand {command:?}\n{}", usage())),
    };

    let mut config = Config::from_env();
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            return Err(usage().to_owned());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--addr" => config.addr = value,
            "--token" => config.token = (!value.is_empty()).then_some(value),
            "--home" => config.home = value.into(),
            "--display" => config.display = value,
            "--screen" => config.screen = value,
            _ => return Err(format!("unknown flag {flag:?}\n{}", usage())),
        }
    }
    Ok((command, config))
}

fn main() {
    let (command, config) = match parse() {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let result = match command {
        Command::Boot => boot::run(config),
        Command::Serve => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
            .and_then(|runtime| runtime.block_on(serve::run(App::new(config)))),
    };
    if let Err(error) = result {
        eprintln!("toad-computer: {error}");
        std::process::exit(1);
    }
}
