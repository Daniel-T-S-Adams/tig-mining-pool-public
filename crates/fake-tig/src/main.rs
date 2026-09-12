//! Binary entry point. Binds loopback only; port 0 selects an ephemeral port
//! and the bound address is printed as a JSON line so scripts can parse it.

use fake_tig::{Config, build_world, router};

fn usage() -> ! {
    eprintln!(
        "usage: fake-tig --fixture <dir> [--port <u16, default 0>] \
         [--api-key <key>] [--confirm-delay <blocks, default 1>] \
         [--player <id: serve the fixture's pool player as this id>]"
    );
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut fixture: Option<String> = None;
    let mut port: u16 = 0;
    let mut api_key: Option<String> = None;
    let mut confirm_delay: u32 = 1;
    let mut player: Option<String> = None;
    while let Some(flag) = args.next() {
        let Some(value) = args.next() else { usage() };
        match flag.as_str() {
            "--fixture" => fixture = Some(value),
            "--port" => match value.parse() {
                Ok(p) => port = p,
                Err(_) => usage(),
            },
            "--api-key" => api_key = Some(value),
            "--confirm-delay" => match value.parse() {
                Ok(d) => confirm_delay = d,
                Err(_) => usage(),
            },
            "--player" => player = Some(value),
            _ => usage(),
        }
    }
    let Some(fixture) = fixture else { usage() };

    let mut cfg = Config::new(fixture);
    if let Some(key) = api_key {
        cfg.api_key = key;
    }
    cfg.confirm_delay = confirm_delay;
    cfg.pool_player_id = player;

    let world = match build_world(cfg) {
        Ok(world) => world,
        Err(err) => {
            eprintln!("fake-tig: {err}");
            std::process::exit(1);
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("fake-tig: cannot start runtime: {err}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(err) => {
                eprintln!("fake-tig: cannot bind 127.0.0.1:{port}: {err}");
                std::process::exit(1);
            }
        };
        match listener.local_addr() {
            Ok(addr) => println!("{{\"listening\":\"{addr}\"}}"),
            Err(err) => {
                eprintln!("fake-tig: {err}");
                std::process::exit(1);
            }
        }
        if let Err(err) = axum::serve(listener, router(world)).await {
            eprintln!("fake-tig: server error: {err}");
            std::process::exit(1);
        }
    });
}
