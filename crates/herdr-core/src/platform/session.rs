/// The OS login a process inherited, rather than text restored from its environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct LoginSession {
    pub id: u32,
    pub user: u32,
    /// Unknown provenance never triggers automatic replacement.
    pub gui: Option<bool>,
}

impl LoginSession {
    pub fn replaces(self, core: Option<Self>) -> bool {
        self.gui == Some(true)
            && core.is_some_and(|old| old.gui == Some(true) && old.user == self.user && old.id != self.id)
    }
}

pub fn holder_current(
    holder_build: &str,
    core_build: &str,
    draining: bool,
    core: Option<LoginSession>,
    holder: Option<LoginSession>,
) -> bool {
    holder_build == core_build
        && !draining
        && core.zip(holder).is_none_or(|(core, holder)| core.id == holder.id && core.user == holder.user)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u32, gui: Option<bool>) -> LoginSession {
        LoginSession { id, user: 501, gui }
    }

    #[test]
    fn only_a_new_gui_login_repairs_an_old_gui_core() {
        let gui = session(20, Some(true));
        assert!(gui.replaces(Some(session(10, Some(true)))));
        for old in [None, Some(gui), Some(session(10, Some(false))), Some(session(10, None))] {
            assert!(!gui.replaces(old));
        }
        for launcher in [session(20, Some(false)), session(20, None), LoginSession { user: 502, ..gui }] {
            assert!(!launcher.replaces(Some(session(10, Some(true)))));
        }
    }

    #[test]
    fn a_holder_needs_the_build_session_and_no_prior_drain() {
        let core = Some(session(20, Some(true)));
        assert!(holder_current("same", "same", false, core, core));
        assert!(!holder_current("old", "same", false, core, core));
        assert!(!holder_current("same", "same", true, core, core));
        assert!(!holder_current("same", "same", false, core, Some(session(10, Some(true)))));
        assert!(!holder_current("same", "same", false, core, Some(session(10, Some(false)))));
        assert!(holder_current("same", "same", false, core, None));
        assert!(holder_current("same", "same", false, None, core));
    }
}
