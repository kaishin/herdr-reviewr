use herdr_reviewr::actions::{self, NonUiRun};

fn main() -> anyhow::Result<()> {
    // `NonUiRun` decides both dispatch and the actions' read of which panes run the UI.
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match NonUiRun::from_args(&args) {
        Some(NonUiRun::Action(name)) => std::process::exit(actions::run(&name)),
        Some(NonUiRun::MarkReviewed(paths)) => {
            herdr_reviewr::log::init();
            let n = herdr_reviewr::persist::mark_reviewed(&paths)?;
            let noun = if n == 1 { "file" } else { "files" };
            println!("reviewr: marked {n} {noun} reviewed");
            Ok(())
        }
        Some(NonUiRun::FocusFile(args)) => {
            anyhow::ensure!(
                args.len() == 2 && args[0] == "--focus-file",
                "usage: herdr-reviewr --focus-file <file>"
            );
            herdr_reviewr::control::focus_file(std::path::Path::new(&args[1]))?;
            println!("reviewr: focused {}", args[1].to_string_lossy());
            Ok(())
        }
        None => herdr_reviewr::run(),
    }
}
