pub mod api {
    pub mod action_api {
        include!("api.action.rs");
    }
    pub use action_api as action;
    pub mod command {
        include!("api.command.rs");
    }
    pub mod event_api {
        include!("api.event.rs");
    }
    pub use event_api as event;
    pub mod file {
        include!("api.file.rs");
    }
    pub mod input_mode {
        include!("api.input_mode.rs");
    }
    pub mod key_api {
        include!("api.key.rs");
    }
    pub use key_api as key;
    pub mod message {
        include!("api.message.rs");
    }
    pub mod pipe_message {
        include!("api.pipe_message.rs");
    }
    pub mod plugin_command_api {
        include!("api.plugin_command.rs");
    }
    pub use plugin_command_api as plugin_command;
    pub mod plugin_ids {
        include!("api.plugin_ids.rs");
    }
    pub mod plugin_permission {
        include!("api.plugin_permission.rs");
    }
    pub mod resize {
        include!("api.resize.rs");
    }
    pub mod style {
        include!("api.style.rs");
    }
}
