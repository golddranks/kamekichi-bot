use kamekichi::{ChannelRef, RefRegistry, RoleRef};

fn main() {
    let mut registry = RefRegistry::new();
    let test_ch = ChannelRef::from_stablename("test-channel", &mut registry);
    let hakase_role = RoleRef::from_stablename("hakase", &mut registry);

    let bot = std::env::args().nth(1).unwrap_or_default();
    let result = match bot.as_str() {
        "echo" => kamekichi::run::<testbed::echo::Echo>(),
        "greeter" => kamekichi::run::<testbed::greeter::Greeter>(),
        "logger" => kamekichi::run::<testbed::logger::Logger>(),
        "sentinel" => {
            let bot = testbed::sentinel::Sentinel {
                watched_cn: test_ch,
                alert_ch: test_ch,
            };
            kamekichi::run_bot(bot, registry, "config.json")
        }
        "shibboleth" => {
            let bot = testbed::shibboleth::Shibboleth {
                passphrase: "open sesame".into(),
                role: hakase_role,
            };
            kamekichi::run_bot(bot, registry, "config.json")
        }
        _ => {
            eprintln!("Usage: testbed <echo|greeter|logger|sentinel|shibboleth>");
            std::process::exit(1);
        }
    };
    if let Err(e) = result {
        eprintln!("Fatal: {e}");
        std::process::exit(1);
    }
}
