use log::{error, info};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::protocol::{ChatMessage, ChatPostSegment, Platform};
use crate::streamer::StreamerState;

const YOUTUBE_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:124.0) Gecko/20100101 Firefox/124.0";

const YOUTUBE_MIN_POLL_DELAY_MS: u64 = 200;
const YOUTUBE_MAX_POLL_DELAY_MS: u64 = 3000;
const YOUTUBE_RECONNECT_DELAY_SECS: u64 = 5;

#[derive(Debug, Deserialize)]
struct Thumbnail {
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct Image {
    pub thumbnails: Vec<Thumbnail>,
}

#[derive(Debug, Deserialize)]
struct Emoji {
    pub image: Image,
}

#[derive(Debug, Deserialize)]
struct Run {
    pub text: Option<String>,
    pub emoji: Option<Emoji>,
}

#[derive(Debug, Deserialize)]
struct Message {
    pub runs: Vec<Run>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Author {
    pub simple_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Amount {
    #[allow(dead_code)]
    pub simple_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BadgeIcon {
    pub icon_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorBadgeRenderer {
    pub icon: Option<BadgeIcon>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorBadge {
    pub live_chat_author_badge_renderer: Option<AuthorBadgeRenderer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatDescription {
    pub author_name: Author,
    pub message: Option<Message>,
    #[allow(dead_code)]
    pub purchase_amount_text: Option<Amount>,
    pub header_subtext: Option<Message>,
    pub author_badges: Option<Vec<AuthorBadge>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddChatItemActionItem {
    live_chat_text_message_renderer: Option<ChatDescription>,
    live_chat_paid_message_renderer: Option<ChatDescription>,
    live_chat_paid_sticker_renderer: Option<ChatDescription>,
    live_chat_membership_item_renderer: Option<ChatDescription>,
}

#[derive(Debug, Deserialize)]
struct AddChatItemAction {
    item: AddChatItemActionItem,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    add_chat_item_action: Option<AddChatItemAction>,
}

#[derive(Debug, Deserialize)]
struct InvalidationContinuationData {
    continuation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Continuations {
    invalidation_continuation_data: Option<InvalidationContinuationData>,
}

#[derive(Debug, Deserialize)]
struct LiveChatContinuation {
    continuations: Vec<Continuations>,
    actions: Option<Vec<Action>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContinuationContents {
    live_chat_continuation: LiveChatContinuation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetLiveChat {
    continuation_contents: ContinuationContents,
}

fn create_youtube_segments(chat_description: &ChatDescription) -> Vec<ChatPostSegment> {
    let mut segments: Vec<ChatPostSegment> = Vec::new();
    let mut id: i32 = 0;

    if let Some(header_subtext) = &chat_description.header_subtext {
        for run in &header_subtext.runs {
            if let Some(text) = &run.text {
                for word in text.split_whitespace() {
                    segments.push(ChatPostSegment {
                        id,
                        text: Some(format!("{word} ")),
                        url: None,
                    });
                    id += 1;
                }
            }
            if let Some(emoji) = &run.emoji {
                if let Some(thumbnail) = emoji.image.thumbnails.first() {
                    segments.push(ChatPostSegment {
                        id,
                        text: None,
                        url: Some(thumbnail.url.clone()),
                    });
                    id += 1;
                }
            }
        }
    }

    if let Some(message) = &chat_description.message {
        for run in &message.runs {
            if let Some(text) = &run.text {
                for word in text.split_whitespace() {
                    segments.push(ChatPostSegment {
                        id,
                        text: Some(format!("{word} ")),
                        url: None,
                    });
                    id += 1;
                }
            }
            if let Some(emoji) = &run.emoji {
                if let Some(thumbnail) = emoji.image.thumbnails.first() {
                    segments.push(ChatPostSegment {
                        id,
                        text: None,
                        url: Some(thumbnail.url.clone()),
                    });
                    id += 1;
                }
            }
        }
    }

    segments
}

fn is_youtube_owner(chat_description: &ChatDescription) -> bool {
    chat_description
        .author_badges
        .as_ref()
        .map(|badges| {
            badges.iter().any(|b| {
                b.live_chat_author_badge_renderer
                    .as_ref()
                    .and_then(|r| r.icon.as_ref())
                    .and_then(|i| i.icon_type.as_ref())
                    .is_some_and(|t| t == "OWNER")
            })
        })
        .unwrap_or(false)
}

fn is_youtube_moderator(chat_description: &ChatDescription) -> bool {
    chat_description
        .author_badges
        .as_ref()
        .map(|badges| {
            badges.iter().any(|b| {
                b.live_chat_author_badge_renderer
                    .as_ref()
                    .and_then(|r| r.icon.as_ref())
                    .and_then(|i| i.icon_type.as_ref())
                    .is_some_and(|t| t == "MODERATOR")
            })
        })
        .unwrap_or(false)
}

fn is_youtube_member(chat_description: &ChatDescription) -> bool {
    chat_description
        .author_badges
        .as_ref()
        .map(|badges| {
            badges.iter().any(|b| {
                b.live_chat_author_badge_renderer
                    .as_ref()
                    .and_then(|r| r.icon.as_ref())
                    .and_then(|i| i.icon_type.as_ref())
                    .is_some_and(|t| t == "MEMBER")
            })
        })
        .unwrap_or(false)
}

async fn youtube_get_initial_continuation(
    client: &reqwest::Client,
    video_id: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("https://www.youtube.com/live_chat?is_popout=1&v={video_id}");
    let response = client
        .get(&url)
        .header("User-Agent", YOUTUBE_USER_AGENT)
        .header("Cookie", "CONSENT=YES+1")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!(
            "YouTube live chat page returned status {}",
            response.status()
        )
        .into());
    }

    let body = response.text().await?;
    let re = regex::Regex::new(r#""continuation":"([^"]+)""#)?;
    let captures = re
        .captures(&body)
        .ok_or("No continuation token found in YouTube live chat page")?;
    Ok(captures[1].to_string())
}

async fn youtube_fetch_messages(
    client: &reqwest::Client,
    continuation: &str,
) -> Result<GetLiveChat, Box<dyn std::error::Error + Send + Sync>> {
    let url = "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat";
    let body = serde_json::json!({
        "context": {
            "client": {
                "clientName": "WEB",
                "clientVersion": "2.20210128.02.00"
            }
        },
        "continuation": continuation
    });

    let response = client
        .post(url)
        .header("User-Agent", YOUTUBE_USER_AGENT)
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!(
            "YouTube get_live_chat returned status {}",
            response.status()
        )
        .into());
    }

    let data: GetLiveChat = response.json().await?;
    Ok(data)
}

pub async fn connect_youtube_chat(
    streamer: Arc<Mutex<StreamerState>>,
    video_id: String,
    peer_address: String,
) {
    info!("[{peer_address}] Starting YouTube chat for video: {video_id}");
    let client = reqwest::Client::new();

    loop {
        match youtube_chat_session(&client, &streamer, &video_id, &peer_address).await {
            Ok(()) => {
                break;
            }
            Err(e) => {
                error!("[{peer_address}] YouTube chat error: {e}, reconnecting in {YOUTUBE_RECONNECT_DELAY_SECS}s");
                tokio::time::sleep(tokio::time::Duration::from_secs(
                    YOUTUBE_RECONNECT_DELAY_SECS,
                ))
                .await;
            }
        }
    }

    streamer.lock().await.youtube_running = false;
    info!("[{peer_address}] YouTube chat connection ended");
}

async fn youtube_chat_session(
    client: &reqwest::Client,
    streamer: &Arc<Mutex<StreamerState>>,
    video_id: &str,
    peer_address: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut continuation = youtube_get_initial_continuation(client, video_id).await?;
    info!("[{peer_address}] Got YouTube continuation token, starting message polling");

    let mut delay_ms = YOUTUBE_MAX_POLL_DELAY_MS;

    loop {
        let live_chat = youtube_fetch_messages(client, &continuation).await?;

        let mut message_count = 0;

        if let Some(actions) = &live_chat
            .continuation_contents
            .live_chat_continuation
            .actions
        {
            for action in actions {
                let item = match &action.add_chat_item_action {
                    Some(a) => &a.item,
                    None => continue,
                };

                let descriptions: Vec<&ChatDescription> = [
                    item.live_chat_text_message_renderer.as_ref(),
                    item.live_chat_paid_message_renderer.as_ref(),
                    item.live_chat_paid_sticker_renderer.as_ref(),
                    item.live_chat_membership_item_renderer.as_ref(),
                ]
                .into_iter()
                .flatten()
                .collect();

                for chat_description in descriptions {
                    let segments = create_youtube_segments(chat_description);
                    if segments.is_empty() {
                        continue;
                    }

                    let is_owner = is_youtube_owner(chat_description);
                    let is_moderator = is_youtube_moderator(chat_description);
                    let is_subscriber = is_youtube_member(chat_description);
                    let display_name = chat_description.author_name.simple_text.clone();

                    let mut streamer = streamer.lock().await;

                    let message = ChatMessage {
                        id: streamer.next_chat_message_id(),
                        platform: Platform::YouTube {},
                        message_id: None,
                        display_name: Some(display_name.clone()),
                        user: Some(display_name),
                        user_id: None,
                        user_color: None,
                        user_badges: vec![],
                        segments,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        is_action: false,
                        is_moderator,
                        is_subscriber,
                        is_owner,
                        bits: None,
                    };

                    streamer.append_chat_message(message).await;
                    message_count += 1;
                }
            }
        }

        let new_continuation = live_chat
            .continuation_contents
            .live_chat_continuation
            .continuations
            .iter()
            .find_map(|c| {
                c.invalidation_continuation_data
                    .as_ref()
                    .map(|d| d.continuation.clone())
            })
            .ok_or("No continuation token in YouTube response")?;
        continuation = new_continuation;

        if message_count > 0 {
            delay_ms = delay_ms * 5 / message_count as u64;
        } else {
            delay_ms = YOUTUBE_MAX_POLL_DELAY_MS;
        }
        delay_ms = delay_ms.clamp(YOUTUBE_MIN_POLL_DELAY_MS, YOUTUBE_MAX_POLL_DELAY_MS);

        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_youtube_segments_text_only() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "TestUser".to_string(),
            },
            message: Some(Message {
                runs: vec![Run {
                    text: Some("hello world".to_string()),
                    emoji: None,
                }],
            }),
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text.as_deref(), Some("hello "));
        assert_eq!(segments[1].text.as_deref(), Some("world "));
    }

    #[test]
    fn test_youtube_segments_with_emoji() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "TestUser".to_string(),
            },
            message: Some(Message {
                runs: vec![
                    Run {
                        text: Some("hi ".to_string()),
                        emoji: None,
                    },
                    Run {
                        text: None,
                        emoji: Some(Emoji {
                            image: Image {
                                thumbnails: vec![Thumbnail {
                                    url: "https://example.com/emoji.png".to_string(),
                                }],
                            },
                        }),
                    },
                    Run {
                        text: Some(" bye".to_string()),
                        emoji: None,
                    },
                ],
            }),
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text.as_deref(), Some("hi "));
        assert!(segments[0].url.is_none());
        assert!(segments[1].text.is_none());
        assert_eq!(
            segments[1].url.as_deref(),
            Some("https://example.com/emoji.png")
        );
        assert_eq!(segments[2].text.as_deref(), Some("bye "));
    }

    #[test]
    fn test_youtube_segments_empty_message() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "TestUser".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert!(segments.is_empty());
    }

    #[test]
    fn test_youtube_owner_detection() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "Owner".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: Some(vec![AuthorBadge {
                live_chat_author_badge_renderer: Some(AuthorBadgeRenderer {
                    icon: Some(BadgeIcon {
                        icon_type: Some("OWNER".to_string()),
                    }),
                }),
            }]),
        };
        assert!(is_youtube_owner(&desc));
        assert!(!is_youtube_moderator(&desc));
        assert!(!is_youtube_member(&desc));
    }

    #[test]
    fn test_youtube_moderator_detection() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "Mod".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: Some(vec![AuthorBadge {
                live_chat_author_badge_renderer: Some(AuthorBadgeRenderer {
                    icon: Some(BadgeIcon {
                        icon_type: Some("MODERATOR".to_string()),
                    }),
                }),
            }]),
        };
        assert!(!is_youtube_owner(&desc));
        assert!(is_youtube_moderator(&desc));
    }

    #[test]
    fn test_youtube_no_badges() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "User".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        assert!(!is_youtube_owner(&desc));
        assert!(!is_youtube_moderator(&desc));
        assert!(!is_youtube_member(&desc));
    }

    #[test]
    fn test_youtube_segments_header_subtext() {
        let desc = ChatDescription {
            author_name: Author {
                simple_text: "TestUser".to_string(),
            },
            message: Some(Message {
                runs: vec![Run {
                    text: Some("main message".to_string()),
                    emoji: None,
                }],
            }),
            purchase_amount_text: None,
            header_subtext: Some(Message {
                runs: vec![Run {
                    text: Some("header".to_string()),
                    emoji: None,
                }],
            }),
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text.as_deref(), Some("header "));
        assert_eq!(segments[1].text.as_deref(), Some("main "));
        assert_eq!(segments[2].text.as_deref(), Some("message "));
    }

    #[test]
    fn test_deserialize_youtube_live_chat_response() {
        let json = r#"{
            "continuationContents": {
                "liveChatContinuation": {
                    "continuations": [{
                        "invalidationContinuationData": {
                            "continuation": "next_token"
                        }
                    }],
                    "actions": [{
                        "addChatItemAction": {
                            "item": {
                                "liveChatTextMessageRenderer": {
                                    "authorName": {"simpleText": "TestUser"},
                                    "message": {
                                        "runs": [{"text": "Hello!"}]
                                    }
                                }
                            }
                        }
                    }]
                }
            }
        }"#;
        let data: GetLiveChat = serde_json::from_str(json).unwrap();
        let continuation = &data
            .continuation_contents
            .live_chat_continuation
            .continuations[0];
        assert_eq!(
            continuation
                .invalidation_continuation_data
                .as_ref()
                .unwrap()
                .continuation,
            "next_token"
        );
        let actions = data
            .continuation_contents
            .live_chat_continuation
            .actions
            .as_ref()
            .unwrap();
        assert_eq!(actions.len(), 1);
        let item = &actions[0].add_chat_item_action.as_ref().unwrap().item;
        let renderer = item.live_chat_text_message_renderer.as_ref().unwrap();
        assert_eq!(renderer.author_name.simple_text, "TestUser");
    }
}
