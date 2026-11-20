//! Open Horizon's first-run onboarding, after the welcome and the data question: first an
//! account (an offline name, or Ely.by; Microsoft stays one click away), then how Minecraft's
//! files are found: the player's own Java Edition install, or "let us handle it", which
//! downloads the game from Mojang once a Microsoft account shows it owns it, prepares it, and
//! deletes the download (`backend::clean_downloaded_minecraft`).
use super::line_edit::{self, Key, LineEdit};
use super::*;
use horizon_launcher::account::open::{self as accounts_open, ElyBySignIn};
use horizon_launcher::backend::InstallMethod;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Account,
    Install,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Name,
    Login,
    Password,
    Code,
}

pub(super) struct Onboarding {
    pub(super) step: Step,
    name: LineEdit,
    login: LineEdit,
    password: LineEdit,
    code: LineEdit,
    focus: [FocusHandle; 4],
    focused: Option<Field>,
    /// Ely.by asked for the two-step code.
    needs_code: bool,
    /// An Ely.by sign-in is under way.
    signing_in: bool,
    error: Option<String>,
    /// Ely.by's answer, from its thread.
    rx: Option<std::sync::mpsc::Receiver<Result<ElyBySignIn, String>>>,
}

impl Onboarding {
    pub(super) fn new(cx: &mut App) -> Self {
        let handle = |cx: &mut App, i| cx.focus_handle().tab_index(i).tab_stop(true);
        Self {
            step: Step::Account,
            name: LineEdit::new(String::new(), 16, 24),
            login: LineEdit::new(String::new(), 100, 32),
            password: LineEdit::new(String::new(), 200, 32),
            code: LineEdit::new(String::new(), 10, 12),
            focus: [handle(cx, 0), handle(cx, 1), handle(cx, 2), handle(cx, 3)],
            focused: None,
            needs_code: false,
            signing_in: false,
            error: None,
            rx: None,
        }
    }

    pub(super) fn note_focus(&mut self, window: &Window) {
        self.focused = [Field::Name, Field::Login, Field::Password, Field::Code]
            .into_iter()
            .find(|f| self.focus[*f as usize].is_focused(window));
    }

    fn edit(&mut self, field: Field) -> &mut LineEdit {
        match field {
            Field::Name => &mut self.name,
            Field::Login => &mut self.login,
            Field::Password => &mut self.password,
            Field::Code => &mut self.code,
        }
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

    fn start_elyby_sign_in(&mut self, cx: &mut Context<Self>) {
        let o = &mut self.onboarding;
        if o.signing_in || o.login.text().trim().is_empty() || o.password.text().is_empty() {
            return;
        }
        let (login, password) = (o.login.text().to_owned(), o.password.text().to_owned());
        let code = o.needs_code.then(|| o.code.text().to_owned());
        let (tx, rx) = std::sync::mpsc::channel();
        o.rx = Some(rx);
        o.signing_in = true;
        o.error = None;
        std::thread::spawn(move || {
            let _ = tx.send(accounts_open::elyby_sign_in(
                horizon_addresses::ELYBY_AUTH,
                &login,
                &password,
                code.as_deref(),
            ));
        });
        // Ely.by answers on that thread: look for its answer until it comes.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let waiting = this.update(cx, |this, cx| {
                this.take_elyby_answer();
                cx.notify();
                this.onboarding.signing_in
            });
            if !waiting.unwrap_or(false) {
                break;
            }
        })
        .detach();
    }

    /// Reads Ely.by's answer when it has come (each frame while the sign-in is under way).
    fn take_elyby_answer(&mut self) {
        let Some(answer) = self
            .onboarding
            .rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        else {
            return;
        };
        let o = &mut self.onboarding;
        o.rx = None;
        o.signing_in = false;
        match answer {
            Ok(ElyBySignIn::NeedsCode) => {
                o.needs_code = true;
                o.error = Some(tr("Enter the code from your authenticator app.").into());
            }
            Ok(ElyBySignIn::Done(signed_in, secrets)) => {
                o.password.set(String::new());
                let mut accounts = Accounts::load();
                let kept = accounts
                    .add_elyby(account::store(), signed_in, &secrets)
                    .and_then(|()| accounts.save());
                match kept {
                    Ok(()) => self.onboarding_account_done(),
                    Err(e) => self.onboarding.error = Some(e),
                }
            }
            Err(e) => o.error = Some(tr(&e).to_string()),
        }
    }

    fn onboarding_field(
        &mut self,
        field: Field,
        placeholder: &'static str,
        mask: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        use crate::glass_ui::white;
        let t = self.theme;
        let focused = self.onboarding.focused == Some(field);
        let id: &'static str = match field {
            Field::Name => "onboarding-name",
            Field::Login => "onboarding-login",
            Field::Password => "onboarding-password",
            Field::Code => "onboarding-code",
        };
        let el = div()
            .id(id)
            .track_focus(&self.onboarding.focus[field as usize])
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
                match field {
                    Field::Name => &self.onboarding.name,
                    Field::Login => &self.onboarding.login,
                    Field::Password => &self.onboarding.password,
                    Field::Code => &self.onboarding.code,
                },
                &t,
                focused,
                false,
                mask,
                placeholder,
            ))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                window.focus(&this.onboarding.focus[field as usize], cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                match line_edit::handle(this.onboarding.edit(field), ev, cx) {
                    Key::Enter => match field {
                        Field::Name => this.add_offline_account(),
                        _ => this.start_elyby_sign_in(cx),
                    },
                    Key::Escape => window.blur(cx),
                    Key::Pass => return,
                    _ => {}
                }
                cx.stop_propagation();
                cx.notify();
            }));
        line_edit::mouse(el, cx, move |this: &mut Launcher| {
            this.onboarding.edit(field)
        })
    }

    pub(crate) fn onboarding_screen(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
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
                            .child(self.onboarding_field(Field::Name, "Your player name", false, cx))
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
                        let mut ely = panel()
                            .child(ui::text(15., FontWeight::SEMIBOLD, ink::FULL).child(tr("Ely.by")))
                            .child(self.onboarding_field(Field::Login, "Ely.by name or e-mail", false, cx))
                            .child(self.onboarding_field(Field::Password, "Password", true, cx));
                        if self.onboarding.needs_code {
                            ely = ely.child(self.onboarding_field(Field::Code, "Two-step code", false, cx));
                        }
                        let label = if self.onboarding.signing_in { "Signing in…" } else { "Sign in with Ely.by" };
                        ely.child(
                            ui::button("onboarding-elyby".into(), tr(label).into(), Kind::Secondary, &t)
                                .h(px(40.))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.start_elyby_sign_in(cx);
                                    cx.notify();
                                })),
                        )
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
                            this.finish_onboarding(method);
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
