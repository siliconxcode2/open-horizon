//! Open Horizon's first-run onboarding, after the welcome and the data question: first an
//! account (an offline name, or Ely.by; Microsoft stays one click away), then how Minecraft's
//! files are found: the player's own Java Edition install, or "let us handle it", which
//! downloads the game from Mojang once a Microsoft account shows it owns it, prepares it, and
//! deletes the download (`backend::clean_downloaded_minecraft`).
use super::line_edit::{self, Key, LineEdit};
use super::*;
use horizon_launcher::account::open::{self as accounts_open, ElyBySecrets};
use horizon_launcher::backend::InstallMethod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Account,
    Install,
    /// "I already have Java Edition": which launcher folder holds it.
    Locate,
}

/// A launcher folder found on this computer: its launcher's name, the folder, and whether it
/// holds the Minecraft version the game is made from.
type Detected = Vec<(&'static str, std::path::PathBuf, bool)>;

/// Ely.by's answer to a sign-in, from its thread.
type ElyByAnswer = Result<(account::Account, ElyBySecrets), String>;

pub(super) struct Onboarding {
    pub(super) step: Step,
    name: LineEdit,
    focus: FocusHandle,
    focused: bool,
    /// The Ely.by page the browser was sent to, while that sign-in is under way.
    elyby_url: Option<String>,
    /// Set to stop waiting for the browser.
    elyby_cancel: Arc<AtomicBool>,
    error: Option<String>,
    rx: Option<std::sync::mpsc::Receiver<ElyByAnswer>>,
    /// The launcher folders found, once looked for.
    detected: Option<Detected>,
    /// The folder the player picked, and what was found in it (the game's jar, or what it lacks).
    chosen: Option<(
        std::path::PathBuf,
        Option<Result<std::path::PathBuf, String>>,
    )>,
}

impl Onboarding {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            step: Step::Account,
            name: LineEdit::new(String::new(), 16, 24),
            focus: cx.focus_handle().tab_index(0).tab_stop(true),
            focused: false,
            elyby_url: None,
            elyby_cancel: Arc::new(AtomicBool::new(false)),
            error: None,
            rx: None,
            detected: None,
            chosen: None,
        }
    }

    pub(super) fn note_focus(&mut self, window: &Window) {
        self.focused = self.focus.is_focused(window);
    }

    fn signing_in(&self) -> bool {
        self.rx.is_some()
    }
}

impl Launcher {
    /// Whether the onboarding covers the window: a player who hasn't been through it, once the
    /// opening, the welcome and the data question are done.
    pub(crate) fn onboarding_open(&self) -> bool {
        !self.settings.onboarded
            && self.opening.is_none()
            && !self.welcoming()
            && !self.asking_consent()
    }

    /// Ends the onboarding: the owner screen's first-run notice is not needed after it.
    fn finish_onboarding(&mut self, method: InstallMethod) {
        self.settings.onboarded = true;
        self.settings.owner_notice_seen = true;
        self.settings.install_method = Some(method);
        self.save_settings();
        self.accounts = Accounts::load();
        self.owner = Some(self.accounts.has_owner(account::store()));
        // "Let us handle it" needs a Microsoft account that owns the game, once, for Mojang's
        // download; without one the sign-in opens now.
        if method == InstallMethod::Managed && !self.accounts.has_microsoft_owner(account::store())
        {
            self.sign_in_to_play();
        }
    }

    fn onboarding_account_done(&mut self) {
        self.onboarding.error = None;
        self.onboarding.step = Step::Install;
        self.accounts = Accounts::load();
        self.owner = Some(self.accounts.has_owner(account::store()));
    }

    fn add_offline_account(&mut self) {
        let mut accounts = Accounts::load();
        let added = accounts
            .add_offline(self.onboarding.name.text())
            .and_then(|_| accounts.save());
        match added {
            Ok(()) => self.onboarding_account_done(),
            Err(e) => self.onboarding.error = Some(tr(&e).to_string()),
        }
    }

    /// Opens Ely.by's sign-in page in the browser (OAuth 2.0 with PKCE, `account::open`) and
    /// waits on a thread for Ely.by to send the player back.
    fn start_elyby_sign_in(&mut self, cx: &mut Context<Self>) {
        if self.onboarding.signing_in() {
            return;
        }
        let started =
            match accounts_open::elyby_oauth_start(horizon_addresses::ELYBY_OAUTH_AUTHORIZE) {
                Ok(started) => started,
                Err(e) => {
                    self.onboarding.error = Some(e);
                    return;
                }
            };
        cx.open_url(&started.url);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let o = &mut self.onboarding;
        o.elyby_url = Some(started.url.clone());
        o.elyby_cancel = cancel.clone();
        o.rx = Some(rx);
        o.error = None;
        std::thread::spawn(move || {
            let _ = tx.send(started.finish(
                horizon_addresses::ELYBY_OAUTH_TOKEN,
                horizon_addresses::ELYBY_ACCOUNT_INFO,
                &|| cancel.load(Ordering::Relaxed),
            ));
        });
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let waiting = this.update(cx, |this, cx| {
                this.take_elyby_answer();
                cx.notify();
                this.onboarding.signing_in()
            });
            if !waiting.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    fn cancel_elyby_sign_in(&mut self) {
        self.onboarding.elyby_cancel.store(true, Ordering::Relaxed);
    }

    /// Takes Ely.by's answer when it has come.
    fn take_elyby_answer(&mut self) {
        let Some(answer) = self
            .onboarding
            .rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        else {
            return;
        };
        self.onboarding.rx = None;
        self.onboarding.elyby_url = None;
        match answer {
            Ok((signed_in, secrets)) => {
                let mut accounts = Accounts::load();
                let kept = accounts
                    .add_elyby(account::store(), signed_in, &secrets)
                    .and_then(|()| accounts.save());
                match kept {
                    Ok(()) => self.onboarding_account_done(),
                    Err(e) => self.onboarding.error = Some(e),
                }
            }
            Err(e) => self.onboarding.error = Some(tr(&e).to_string()),
        }
    }

    /// Opens the Locate step and looks for launcher folders on a thread.
    fn start_locate(&mut self, cx: &mut Context<Self>) {
        self.onboarding.step = Step::Locate;
        if self.onboarding.detected.is_some() {
            return;
        }
        let task = cx
            .background_executor()
            .spawn(async { backend::detected_minecraft() });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                // The first folder that has the game is picked for the player.
                if this.onboarding.chosen.is_none() {
                    if let Some((_, dir, _)) = found.iter().find(|(_, _, has)| *has) {
                        let dir = dir.clone();
                        this.check_folder(dir, cx);
                    }
                }
                this.onboarding.detected = Some(found);
                cx.notify();
            });
        })
        .detach();
    }

    /// Picks `dir` and looks for the game in it (in depth, any launcher's layout).
    fn check_folder(&mut self, dir: std::path::PathBuf, cx: &mut Context<Self>) {
        self.onboarding.chosen = Some((dir.clone(), None));
        let task = cx
            .background_executor()
            .spawn(async move { backend::locate_minecraft(&dir) });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some((_, result)) = &mut this.onboarding.chosen {
                    *result = Some(found);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_minecraft_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr("Choose").into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await {
                if let Some(path) = paths.pop() {
                    let _ = this.update(cx, |this, cx| {
                        this.check_folder(path, cx);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Prepares from the folder found, from now on.
    fn use_chosen_folder(&mut self) {
        if let Some((dir, Some(Ok(_)))) = self.onboarding.chosen.clone() {
            self.settings.minecraft_dir = Some(dir);
            self.finish_onboarding(InstallMethod::Existing);
        }
    }

    fn onboarding_name_field(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        use crate::glass_ui::white;
        let t = self.theme;
        let focused = self.onboarding.focused;
        let el = div()
            .id("onboarding-name")
            .track_focus(&self.onboarding.focus)
            .flex()
            .items_center()
            .w_full()
            .h(px(40.))
            .px(px(12.))
            .overflow_hidden()
            .rounded(px(horizon_theme::glass::radius::ROW))
            .border_1()
            .border_color(if focused { t.accent } else { white(0.14) })
            .bg(white(0.06))
            .text_size(px(14.))
            .cursor_text()
            .child(line_edit::content(
                &self.onboarding.name,
                &t,
                focused,
                false,
                false,
                "Your player name",
            ))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                window.focus(&this.onboarding.focus, cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match line_edit::handle(&mut this.onboarding.name, ev, cx) {
                    Key::Enter => this.add_offline_account(),
                    Key::Escape => window.blur(cx),
                    Key::Pass => return,
                    _ => {}
                }
                cx.stop_propagation();
                cx.notify();
            }));
        line_edit::mouse(el, cx, |this: &mut Launcher| &mut this.onboarding.name)
    }

    pub(crate) fn onboarding_screen(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        // A player who already has an account (added from the CLI, or kept from before) goes
        // straight to how the game's files are found.
        if self.onboarding.step == Step::Account
            && !self.onboarding.signing_in()
            && !self.accounts.accounts.is_empty()
        {
            self.onboarding.step = Step::Install;
        }
        use crate::glass_ui::{self as ui, white};
        use horizon_theme::glass::{ink, radius};
        let t = self.theme;
        let heading = |caption: &'static str, title: &'static str| {
            div()
                .flex()
                .flex_col()
                .child(ui::text(13., FontWeight::MEDIUM, ink::QUIET).child(tr(caption)))
                .child(
                    div()
                        .mt(px(8.))
                        .font_family(horizon_theme::glass::DISPLAY_FONT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(28.))
                        .line_height(px(35.))
                        .text_color(white(ink::FULL))
                        .child(tr(title)),
                )
        };
        let body =
            |text: &'static str| ui::text(14., FontWeight::NORMAL, ink::BODY).child(tr(text));
        let panel = || {
            div()
                .mt(px(16.))
                .p(px(18.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .rounded(px(radius::POPOVER))
                .bg(white(0.045))
        };
        let mut card = ui::dialog_plate(radius::MODAL)
            .id("onboarding-card")
            .occlude()
            .flex()
            .flex_col()
            .w(px(560.))
            .max_w_full()
            .p(px(32.));
        match self.onboarding.step {
            Step::Account => {
                card = card
                    .child(heading("Welcome to Open Horizon", "Create your account"))
                    .child(body("Pick a name to play offline, or sign in with Ely.by. You can add more accounts later in Settings."))
                    .child(
                        panel()
                            .child(ui::text(15., FontWeight::SEMIBOLD, ink::FULL).child(tr("Offline")))
                            .child(self.onboarding_name_field(cx))
                            .child(
                                ui::button("onboarding-offline".into(), tr("Play offline").into(), Kind::Primary, &t)
                                    .h(px(40.))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.add_offline_account();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child({
                        let ely = panel()
                            .child(ui::text(15., FontWeight::SEMIBOLD, ink::FULL).child(tr("Ely.by")))
                            .child(ui::text(13., FontWeight::NORMAL, ink::BODY).child(tr(
                                "Sign in on Ely.by's own page in your browser. Open Horizon never sees your password.",
                            )));
                        match self.onboarding.elyby_url.clone() {
                            None => ely.child(
                                ui::button("onboarding-elyby".into(), tr("Sign in with Ely.by").into(), Kind::Secondary, &t)
                                    .h(px(40.))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.start_elyby_sign_in(cx);
                                        cx.notify();
                                    })),
                            ),
                            Some(url) => ely
                                .child(ui::text(13., FontWeight::NORMAL, ink::QUIET).child(tr(
                                    "Waiting for Ely.by in your browser… the window goes on by itself.",
                                )))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(8.))
                                        .child(
                                            ui::button("onboarding-elyby-again".into(), tr("Open the page again").into(), Kind::Secondary, &t)
                                                .h(px(40.))
                                                .on_click(move |_, _, cx| cx.open_url(&url)),
                                        )
                                        .child(
                                            ui::button("onboarding-elyby-cancel".into(), tr("Cancel").into(), Kind::Ghost, &t)
                                                .h(px(40.))
                                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                                    this.cancel_elyby_sign_in();
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        }
                    });
                if let Some(e) = &self.onboarding.error {
                    card = card.child(
                        ui::text(13., FontWeight::NORMAL, ink::FULL)
                            .mt(px(12.))
                            .text_color(gpui::rgb(0xff8a80))
                            .child(e.clone()),
                    );
                }
                card = card.child(
                    div().flex().gap(px(8.)).mt(px(20.)).child(
                        ui::button(
                            "onboarding-microsoft".into(),
                            tr("Sign in with Microsoft instead").into(),
                            Kind::Ghost,
                            &t,
                        )
                        .h(px(40.))
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.onboarding.step = Step::Install;
                                this.sign_in_to_play();
                                cx.notify();
                            },
                        )),
                    ),
                );
            }
            Step::Install => {
                let choice = |id: &'static str,
                              title: &'static str,
                              about: &'static str,
                              method: InstallMethod| {
                    div()
                        .id(id)
                        .mt(px(16.))
                        .p(px(18.))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .rounded(px(radius::POPOVER))
                        .bg(white(0.045))
                        .border_1()
                        .border_color(white(0.10))
                        .cursor_pointer()
                        .hover(|s| s.bg(white(0.08)))
                        .child(ui::text(16., FontWeight::SEMIBOLD, ink::FULL).child(tr(title)))
                        .child(ui::text(13., FontWeight::NORMAL, ink::BODY).child(tr(about)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            match method {
                                InstallMethod::Existing => this.start_locate(cx),
                                InstallMethod::Managed => this.finish_onboarding(method),
                            }
                            cx.notify();
                        }))
                };
                card = card
                    .child(heading("Minecraft's files", "How should Open Horizon get the game?"))
                    .child(body("Open Horizon is built from Minecraft: Java Edition's own files, prepared on this computer."))
                    .child(choice(
                        "onboarding-existing",
                        "I already have Minecraft: Java Edition",
                        "Open Horizon prepares the game from your Java Edition install. Nothing is downloaded from Mojang, and any account here can play.",
                        InstallMethod::Existing,
                    ))
                    .child(choice(
                        "onboarding-managed",
                        "Let Open Horizon handle everything",
                        "Open Horizon downloads Minecraft from Mojang, prepares it, then deletes the download. Mojang's download needs a one-time sign-in with the Microsoft account that owns Minecraft.",
                        InstallMethod::Managed,
                    ));
            }
            Step::Locate => {
                let version = backend::current_pins()
                    .map(|p| p.version)
                    .unwrap_or_default();
                card = card
                    .child(heading("Minecraft's files", "Find your Minecraft folder"))
                    .child(ui::text(14., FontWeight::NORMAL, ink::BODY).mt(px(10.)).child(trf(
                        "Pick the folder your launcher keeps Minecraft in: .minecraft for the official launcher, Badlion, Feather or TLauncher, .lunarclient for Lunar Client, or any other launcher's. It needs Minecraft {version}: play it once there if it isn't.",
                        &[("version", &version)],
                    )));
                let mut list = div().mt(px(16.)).flex().flex_col().gap(px(8.));
                match &self.onboarding.detected {
                    None => {
                        list = list.child(
                            ui::text(13., FontWeight::NORMAL, ink::QUIET)
                                .child(tr("Looking for launchers on this computer…")),
                        )
                    }
                    Some(found) if found.is_empty() => {
                        list = list.child(ui::text(13., FontWeight::NORMAL, ink::QUIET).child(tr(
                            "No launcher folder found in the usual places: choose yours.",
                        )))
                    }
                    Some(found) => {
                        for (i, (name, dir, has)) in found.iter().enumerate() {
                            let picked = self
                                .onboarding
                                .chosen
                                .as_ref()
                                .is_some_and(|(chosen, _)| chosen == dir);
                            let status = if *has {
                                trf("Has Minecraft {version}", &[("version", &version)])
                            } else {
                                trf("No Minecraft {version} here", &[("version", &version)])
                            };
                            let target = dir.clone();
                            list = list.child(
                                div()
                                    .id(("onboarding-folder", i))
                                    .p(px(12.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .rounded(px(radius::ROW))
                                    .bg(white(if picked { 0.10 } else { 0.045 }))
                                    .border_1()
                                    .border_color(if picked { t.accent } else { white(0.08) })
                                    .cursor_pointer()
                                    .hover(|s| s.bg(white(0.08)))
                                    .child(
                                        ui::text(14., FontWeight::SEMIBOLD, ink::FULL)
                                            .child(format!("{} — {status}", capitalized(tr(name)))),
                                    )
                                    .child(
                                        ui::text(12., FontWeight::NORMAL, ink::QUIET)
                                            .child(dir.display().to_string()),
                                    )
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.check_folder(target.clone(), cx);
                                        cx.notify();
                                    })),
                            );
                        }
                    }
                }
                card = card.child(list);
                if let Some((dir, result)) = &self.onboarding.chosen {
                    let (text, color) = match result {
                        None => (
                            trf("Looking in {dir}…", &[("dir", &dir.display().to_string())]),
                            None,
                        ),
                        Some(Ok(jar)) => (
                            trf(
                                "Found Minecraft {version}: {jar}",
                                &[("version", &version), ("jar", &jar.display().to_string())],
                            ),
                            Some(0x8be9a8),
                        ),
                        Some(Err(why)) => {
                            (trf("Not usable: {why}", &[("why", why)]), Some(0xff8a80))
                        }
                    };
                    let mut line = ui::text(13., FontWeight::NORMAL, ink::BODY)
                        .mt(px(12.))
                        .child(text);
                    if let Some(color) = color {
                        line = line.text_color(gpui::rgb(color));
                    }
                    card = card.child(line);
                }
                let ready = matches!(self.onboarding.chosen, Some((_, Some(Ok(_)))));
                let mut use_it = ui::button(
                    "onboarding-use-folder".into(),
                    tr("Use this folder").into(),
                    Kind::Primary,
                    &t,
                )
                .h(px(40.));
                use_it = if ready {
                    use_it.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.use_chosen_folder();
                        cx.notify();
                    }))
                } else {
                    use_it.opacity(0.5)
                };
                card = card.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .mt(px(20.))
                        .child(use_it)
                        .child(
                            ui::button(
                                "onboarding-choose-folder".into(),
                                tr("Choose folder…").into(),
                                Kind::Secondary,
                                &t,
                            )
                            .h(px(40.))
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| {
                                    this.choose_minecraft_folder(cx);
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            ui::button(
                                "onboarding-back".into(),
                                tr("Back").into(),
                                Kind::Ghost,
                                &t,
                            )
                            .h(px(40.))
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| {
                                    this.onboarding.step = Step::Install;
                                    cx.notify();
                                },
                            )),
                        ),
                );
            }
        }
        div()
            .id("onboarding")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .occlude()
            .child(ui::scrim_backdrop())
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(ui::scrim()),
            )
            .child(card)
    }
}

/// `text` with its first letter upper-case ("the official launcher" heads a row).
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
