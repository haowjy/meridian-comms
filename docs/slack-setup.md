# Setting up the Slack app

`meridian-comms` talks to Slack as its own app, with a bot user named
`<owner>-agents` (for Jimmy, `@jimmy-agents`). The app connects over Socket Mode: the
daemon opens an outbound websocket, so nothing listens on a public port. The app has
no user token, so it can't read anyone's DMs, and it sees only the channels its bot is
invited to.

`meridian-comms setup` prints the manifest below with the owner's name filled in.

## 1. Create the app from the manifest

1. Open <https://api.slack.com/apps>, choose **Create New App**, then **From a
   manifest**, and pick the workspace.
2. Choose the JSON tab, replace its contents with the manifest, and create the app.

```json
{
  "display_information": {
    "name": "jimmy-agents",
    "description": "Hands Slack messages to Jimmy's running agent sessions, and posts their answers."
  },
  "features": {
    "bot_user": { "display_name": "jimmy-agents", "always_online": true }
  },
  "oauth_config": {
    "scopes": {
      "bot": [
        "channels:history",
        "channels:read",
        "groups:history",
        "groups:read",
        "chat:write",
        "reactions:write"
      ]
    }
  },
  "settings": {
    "event_subscriptions": {
      "bot_events": ["message.channels", "message.groups"]
    },
    "interactivity": { "is_enabled": true },
    "org_deploy_enabled": false,
    "socket_mode_enabled": true,
    "token_rotation_enabled": false
  }
}
```

What each scope is for:

| Scope | Used for |
|---|---|
| `channels:history`, `groups:history` | Message events, and reading a thread or a channel's recent messages (`read`, recovery) in public and private channels |
| `channels:read`, `groups:read` | Checking at startup whether a configured channel is shared with another organization |
| `chat:write` | Posting in threads the bot is in |
| `reactions:write` | The 👀 reaction on a message that woke someone |

## 2. Make the app-level token

Socket Mode needs a second token. Under **Basic Information**, **App-Level Tokens**,
choose **Generate Token and Scopes**, name it `socket`, add the scope
`connections:write`, and generate. It starts with `xapp-`.

## 3. Install the app and copy the bot token

Under **Install App**, install it to the workspace. Copy the **Bot User OAuth Token**;
it starts with `xoxb-`.

## 4. Put both tokens in `slack.env`

```sh
mkdir -p ~/.config/meridian-comms
install -m 600 /dev/null ~/.config/meridian-comms/slack.env
"${EDITOR:-nano}" ~/.config/meridian-comms/slack.env
```

with these two lines:

```sh
SLACK_BOT_TOKEN=xoxb-...
SLACK_APP_TOKEN=xapp-...
```

The daemon refuses to start if the file is readable by anyone but you. Don't paste the
tokens into a chat or an agent session.

## 5. Invite the bot to each channel

In each channel the config lists, run `/invite @jimmy-agents`. The bot sees nothing in
a channel it isn't in.

## 6. Note the IDs the config needs

- **Channel IDs:** open the channel, click its name, and copy the ID at the bottom of
  the About tab (it starts with `C`).
- **People's member IDs:** open a profile, choose **⋮**, then **Copy member ID** (it
  starts with `U`). You need your own, your cofounder's, and Amp's bot user's.
