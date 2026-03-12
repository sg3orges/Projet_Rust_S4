use gtk::prelude::*;
use gtk::{Align, Button, Label, Orientation, Settings, Window, WindowType, Box as GtkBox};
use std::cell::Cell;
use std::rc::Rc;

use crate::interface;

pub fn run() {
    if gtk::init().is_err() {
        return;
    }

    // Applique le même thème sombre que l'interface principale.
    if let Some(settings) = Settings::get_default() {
        settings.set_property_gtk_application_prefer_dark_theme(true);
    }

    let window = Window::new(WindowType::Toplevel);
    window.set_title("MixRust");
    window.set_default_size(420, 240);

    let container = GtkBox::new(Orientation::Vertical, 18);
    container.set_margin_top(32);
    container.set_margin_bottom(32);
    container.set_margin_start(32);
    container.set_margin_end(32);
    container.set_valign(Align::Center);

    let title = Label::new(None);
    title.set_markup("<span size=\"xx-large\" weight=\"bold\">MixRust</span>");
    title.set_halign(Align::Center);

    let launch_btn = Button::with_label("Lancer l'interface");
    launch_btn.set_halign(Align::Center);

    let quit_btn = Button::with_label("Quitter");
    quit_btn.set_halign(Align::Center);

    container.pack_start(&title, true, true, 0);
    container.pack_start(&launch_btn, false, false, 0);
    container.pack_start(&quit_btn, false, false, 0);

    window.add(&container);

    let launched = Rc::new(Cell::new(false));

    // Ouvre l'interface principale puis ferme l'écran d'accueil.
    launch_btn.connect_clicked({
        let window = window.clone();
        let launched = launched.clone();
        move |_| {
            launched.set(true);
            let main_window = interface::create_main_window();
            main_window.show_all();
            main_window.present();
            window.close();
        }
    });

    quit_btn.connect_clicked(|_| gtk::main_quit());
    window.connect_delete_event(move |_, _| {
        if !launched.get() {
            gtk::main_quit();
        }
        Inhibit(false)
    });

    window.show_all();
    gtk::main();
}
