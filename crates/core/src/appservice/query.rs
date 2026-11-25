/// Endpoints for querying user IDs and room aliases
///
/// `GET /_matrix/app/*/rooms/{roomAlias}`
///
/// Endpoint to query the existence of a given room alias.
/// `/v1/` ([spec])
///
/// [spec]: https://spec.matrix.org/latest/application-service-api/#get_matrixappv1roomsroomalias
use salvo::oapi::ToParameters;
use serde::Deserialize;
use url::Url;

use crate::OwnedRoomAliasId;
use crate::sending::{SendError, SendRequest, SendResult};

// const METADATA: Metadata = metadata! {
//     method: GET,
//     rate_limited: false,
//     authentication: AccessToken,
//     history: {
//         1.0 => "/_matrix/app/v1/rooms/:room_alias",
//     }
// };

pub fn query_room_alias_request(
    origin: &str,
    args: QueryRoomAliasReqArgs,
) -> SendResult<SendRequest> {
    let mut url = Url::parse(origin)?;
    // Path segments percent-encode the alias, whose leading `#` would
    // otherwise start a URL fragment.
    url.path_segments_mut()
        .map_err(|()| SendError::other("appservice URL cannot be used as a base URL"))?
        .pop_if_empty()
        .extend(["_matrix", "app", "v1", "rooms", args.room_alias.as_str()]);
    Ok(crate::sending::get(url))
}

/// Request type for the `query_room_alias` endpoint.
#[derive(ToParameters, Deserialize, Debug)]
pub struct QueryRoomAliasReqArgs {
    /// The room alias being queried.
    #[salvo(parameter(parameter_in = Path))]
    pub room_alias: OwnedRoomAliasId,
}

// /// `GET /_matrix/app/*/users/{user_id}`
// ///  "/_matrix/app/v1/users/:user_id
// ///
// /// Endpoint to query the existence of a given user ID.
// /// `/v1/` ([spec])
// ///
// /// [spec]: https://spec.matrix.org/latest/application-service-api/#get_matrixappv1usersuser_id

// /// Request type for the `query_user_id` endpoint.

// #[derive(ToParameters, Deserialize, Debug)]
// pub struct QueryUseridReqArgs {
//     /// The user ID being queried.
//     #[salvo(parameter(parameter_in = Path))]
//     pub user_id: OwnedUserId,
// }

#[cfg(test)]
mod tests {
    use super::{QueryRoomAliasReqArgs, query_room_alias_request};

    fn request(origin: &str) -> reqwest::Request {
        query_room_alias_request(
            origin,
            QueryRoomAliasReqArgs {
                room_alias: "#portal:example.org".try_into().unwrap(),
            },
        )
        .unwrap()
        .into_inner()
    }

    #[test]
    fn room_alias_query_is_a_get_with_the_alias_in_the_path() {
        for origin in [
            "http://bridge:29317",
            "http://bridge:29317/",
            "http://relay/prefix",
        ] {
            let request = request(origin);
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(request.url().fragment(), None);
            assert!(
                request
                    .url()
                    .path()
                    .ends_with("/_matrix/app/v1/rooms/%23portal:example.org"),
                "{}",
                request.url()
            );
            assert!(!request.url().path().contains("//"), "{}", request.url());
        }
        assert_eq!(
            request("http://relay/prefix").url().path(),
            "/prefix/_matrix/app/v1/rooms/%23portal:example.org"
        );
    }
}
