use tokio::sync::mpsc::UnboundedSender;

pub enum TrayCommand {
    ToggleMainWindow,
    ShowMainWindow,
    Quit,
}

struct Tray {
    commands: UnboundedSender<TrayCommand>,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "oplus-connect".into()
    }

    fn icon_name(&self) -> String {
        "oplus-connect".into()
    }

    fn title(&self) -> String {
        "OPLUS 互联".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.commands.send(TrayCommand::ToggleMainWindow);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};

        vec![
            StandardItem {
                label: "显示窗口".into(),
                activate: Box::new(|tray: &mut Tray| {
                    let _ = tray.commands.send(TrayCommand::ShowMainWindow);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "退出应用".into(),
                activate: Box::new(|tray: &mut Tray| {
                    let _ = tray.commands.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Starts the tray service and forwards menu actions to GPUI.
pub fn start(commands: UnboundedSender<TrayCommand>) {
    std::thread::Builder::new()
        .name("oplus-connect-tray".into())
        .spawn(move || {
            use ksni::blocking::TrayMethods;

            let tray = Tray { commands };
            match tray.spawn() {
                Ok(_handle) => {
                    // Keep the service handle and thread alive for the process lifetime.
                    std::thread::park();
                }
                Err(error) => {
                    tracing::warn!("system tray unavailable: {error}");
                }
            }
        })
        .expect("failed to start system tray thread");
}
