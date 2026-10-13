//! The `:whois` command, which shows what we know about a user.
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::presence::PresenceState;

use crate::prelude::*;

/// Information gathered about a user for `:whois`.
#[derive(Debug, Eq, PartialEq)]
pub struct Whois {
    pub user_id: OwnedUserId,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub presence: Option<PresenceState>,
    /// `None` when the user hasn't set up cross-signing, or we couldn't check.
    pub verified: Option<bool>,
    /// How many rooms we can't tell whether the user is in, since their members aren't loaded.
    pub unknown_rooms: usize,
    pub dm: Option<String>,
    /// Shared rooms, along with the user's display name in each when it differs from their
    /// global one.
    pub rooms: Vec<(String, Option<String>)>,
}

impl Whois {
    pub fn show(&self) -> String {
        let mut lines = vec![match &self.display_name {
            Some(name) => format!("{name} ({})", self.user_id),
            None => self.user_id.to_string(),
        }];

        if let Some(avatar) = &self.avatar_url {
            lines.push(format!("Avatar: {avatar}"));
        }

        if let Some(presence) = &self.presence {
            lines.push(format!("Presence: {presence}"));
        }

        lines.push(
            match self.verified {
                Some(true) => "Identity: verified",
                Some(false) => "Identity: not verified",
                None => "Identity: unknown (no cross-signing keys found)",
            }
            .into(),
        );

        lines.push(match &self.dm {
            Some(dm) => format!("Direct message: {dm} (open with :tabedit {})", self.user_id),
            None => "Direct message: none".into(),
        });

        lines.push(format!("Shared rooms ({}):", self.rooms.len()));
        if self.unknown_rooms > 0 {
            lines.push(format!(
                "  (and maybe some of {} rooms whose members haven't been loaded; open :members there)",
                self.unknown_rooms
            ));
        }
        for (room, name) in &self.rooms {
            match name {
                Some(name) => lines.push(format!("  {room} (as {name:?})")),
                None => lines.push(format!("  {room}")),
            }
        }

        lines.join("\n")
    }
}

/// Look up what we know about a user, fetching their profile from the homeserver.
pub async fn whois(user_id: OwnedUserId, store: &mut ProgramStore) -> IambResult<EditInfo> {
    let client = store.application.worker.client.clone();

    let profile = client.account().fetch_user_profile_of(&user_id).await;
    let field = |name| {
        profile
            .as_ref()
            .ok()
            .and_then(|profile| profile.get(name))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
    };
    let display_name = field("displayname");
    let avatar_url = field("avatar_url");

    // Ask the homeserver if we don't have their keys, which happens when we share no encrypted
    // rooms with them.
    let encryption = client.encryption();
    let identity = match encryption.get_user_identity(&user_id).await {
        Ok(Some(identity)) => Some(identity),
        Ok(None) | Err(_) => encryption.request_user_identity(&user_id).await.ok().flatten(),
    };
    let verified = identity.map(|identity| identity.is_verified());

    let dm = client
        .get_dm_room(&user_id)
        .map(|room| store.application.get_room_title(room.room_id()));

    let mut rooms = vec![];
    let mut unknown_rooms = 0;
    let joined = store
        .application
        .sync_info
        .rooms
        .iter()
        .chain(store.application.sync_info.dms.iter());
    for room in joined {
        // Members are lazily loaded, so only rooms whose full list we have can rule them out.
        let Ok(Some(member)) = room.get_member_no_sync(&user_id).await else {
            if !room.are_members_synced() {
                unknown_rooms += 1;
            }
            continue;
        };

        if !matches!(member.membership(), MembershipState::Join) {
            continue;
        }

        let name = member
            .display_name()
            .filter(|name| Some(*name) != display_name.as_deref())
            .map(ToOwned::to_owned);
        rooms.push((store.application.get_room_title(room.room_id()), name));
    }
    rooms.sort();

    let whois = Whois {
        presence: store.application.presences.get(&user_id).cloned(),
        user_id,
        display_name,
        avatar_url,
        verified,
        dm,
        rooms,
        unknown_rooms,
    };

    Ok(Some(InfoMessage::Pager(whois.show())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;

    #[test]
    fn test_show() {
        let whois = Whois {
            user_id: user_id!("@alice:example.com").to_owned(),
            display_name: Some("Alice".into()),
            presence: Some(PresenceState::Online),
            verified: Some(true),
            dm: Some("Alice".into()),
            rooms: vec![
                ("Lobby".into(), None),
                ("Work".into(), Some("Alice (OOO)".into())),
            ],
            avatar_url: None,
            unknown_rooms: 0,
        };

        assert_eq!(
            whois.show(),
            "Alice (@alice:example.com)\n\
             Presence: online\n\
             Identity: verified\n\
             Direct message: Alice (open with :tabedit @alice:example.com)\n\
             Shared rooms (2):\n  \
             Lobby\n  \
             Work (as \"Alice (OOO)\")"
        );

        let whois = Whois {
            user_id: user_id!("@bob:example.com").to_owned(),
            display_name: None,
            avatar_url: None,
            presence: None,
            verified: None,
            dm: None,
            rooms: vec![],
            unknown_rooms: 3,
        };
        assert_eq!(
            whois.show(),
            "@bob:example.com\n\
             Identity: unknown (no cross-signing keys found)\n\
             Direct message: none\n\
             Shared rooms (0):\n  \
             (and maybe some of 3 rooms whose members haven't been loaded; open :members there)"
        );
    }
}
