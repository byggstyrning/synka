# Interaxo REST API: implementation spec for a desktop file-sync client

Researched 2026-10-20. Scope: OneDrive-style sync of an Interaxo room's documents to a local Windows folder, with "bring your own client id" (BYO) OAuth.

## 0. Sources and legend

The only official API reference is a single static Slate page:
**https://api.interaxo.com/** (byte-identical copy at https://api.interaxo.se/). It loads no OpenAPI/Swagger document. Its only script is `javascripts/all-782fee6e.js` (Slate/jQuery). Probes of `/swagger.json`, `/openapi.json`, `/v2/api-docs`, `/v3/api-docs`, `/swagger-ui.html` and `/v1/...` variants on both hosts return either the same docs HTML or `401`. No public machine-readable spec exists.

Citations use the page anchors, e.g. `[DOC #introduction-environments]` = https://api.interaxo.com/#introduction-environments.

Tags used below:

| Tag | Meaning |
|---|---|
| **[DOC]** | Stated in the Interaxo API reference (anchor given) |
| **[OIDC]** | From the live discovery document https://login.collaboration-sso.com/auth/.well-known/openid-configuration (same content at `/.well-known/openid-configuration`), fetched 2026-10-20 |
| **[AUTH0]** | Generic Auth0 platform behaviour. The IdP is Auth0 (see 2.2). Source: auth0.com docs. Not stated by Interaxo, so confirm for their tenant |
| **[OBSERVED]** | Seen via unauthenticated HTTP probes on 2026-10-20. No sign-in was attempted |
| **[INFERRED]** | Read from the public Interaxo web-app bundle (`https://ix.interaxo.com/resources/js/app-config.js`, `/classic/app.js`). Undocumented, can change without notice |
| **[UNDOCUMENTED]** | Not covered anywhere. Must be verified with Interaxo support or by testing with a real account |

---

## 1. Base URLs, tenants, regions, environments

### 1.1 Hosts [DOC #introduction-environments]

| Environment | Web URL | API base URL |
|---|---|---|
| International (also "older Swedish projects") | `https://ix.interaxo.com` | `https://api.interaxo.com/v1/` |
| Sweden ("new Swedish projects") | `https://ix.interaxo.se` | `https://api.interaxo.se/v1/` |

- "Requests must be sent to the base URL matching where your project is hosted." [DOC]
- "Authentication and audience are identical between environments, but a request sent to the wrong host will fail with a 401 unauthorized." [DOC] The same access token (audience `https://api.interaxo.com/v1/`) is therefore valid on both hosts.
- [INFERRED] Both web apps use the same Auth0 domain (`login.collaboration-sso.com`) and the same audience. `ix.interaxo.com` has `REGION='NO'` and `ix.interaxo.se` has `REGION='SE'`.
- No staging or sandbox environment is documented. [UNDOCUMENTED] The web-app config has dev fallbacks such as `login.collaboration-sso-dev.com` and `id.dev.tribia.io`, but these are internal and not offered to integrators.

### 1.2 Path structure [DOC]

```
{API_BASE}                                  = https://api.interaxo.{com|se}/v1
{API_BASE}/communities                      -> communities (global, no community prefix)
{API_BASE}/{community_id}/...               -> "community context"
{API_BASE}/{community_id}/rooms/{room_id}   -> "room context"
```
- "room context is here the URL context of a room, i.e. `https://api.interaxo.com/v1/{community_id}/rooms/{room_id}`" [DOC #room-content-content-objects-retrieve-a-content-item]
- Community and room ids are human-readable slugs, e.g. `example-corp` and `my-room`. Content ids are UUIDs. [DOC #communities-and-rooms]

### 1.3 How a client finds the right server [UNDOCUMENTED]

There is no discovery endpoint, no region claim in the token, and no per-customer host. The only rule documented is "use the host where the project is hosted" plus "wrong host → 401".

Suggested approach (must be verified):
1. After login, call `GET https://api.interaxo.com/v1/communities` and `GET https://api.interaxo.se/v1/communities` with the same token.
2. Record, per community id, which host returned it. A 200 with a non-empty array means the community is on that host. A 401 or empty array means it is not.
3. Persist `community_id → api_host` and use that host for every call for that community.
4. **Important:** a 401 does not always mean "token invalid". Only start re-authentication if the token is also rejected on the host where the community is known to live, or if the token is actually expired. Otherwise a wrong-host 401 can cause a login loop.
5. Let the user override the environment per account in configuration. The doc's own wording ("older Swedish projects" are on `.com`) means you cannot infer the host from the customer's country.

Not documented: whether `/v1/communities` on one host also lists communities hosted on the other, or returns 401 or an empty list.

---

## 2. Authentication

### 2.1 What Interaxo documents

- Protocol: "OpenID Connect … i.e. OAuth2 over SSL". [DOC #introduction-security]
- All access is in the identity of a user. The API can read only what the user can read and write only where the user can write. "The API user will be set as the owner of items created through the API", including "last modified by" and audit trails. [DOC #introduction-security]
- **Enablement:** "Consumers of the API must first register an application with Interaxo, and will then be given OAuth `client_id` and `client_secret` tokens … Web-based integrations must also provide valid `redirect_uri`s … Contact Interaxo support to register an application." [DOC #introduction-security-enablement]. There is no self-service developer portal. Support: https://support.symetricollaboration.com/ [DOC #support]
- **Audience:** `https://api.interaxo.com/v1/` for both environments. [DOC #introduction-security-authentication]
- **Discovery:** `https://login.collaboration-sso.com/auth/.well-known/openid-configuration` [DOC #introduction-security-authentication]
- **Token endpoint example (client credentials, the only flow with a full example):** [DOC #introduction-security-authentication]
  ```
  curl -d audience=https://api.interaxo.com/v1/ \
       -d grant_type=client_credentials \
       -d client_id=$CLIENT_ID \
       -d client_secret=$CLIENT_SECRET \
       https://login.collaboration-sso.com/oauth/token
  → 200 {"access_token": "my-access-token", ...}
  ```
- **OAuth login parameters** for user login: [DOC #introduction-security-oauth-login-parameters]

  | Parameter | Example | Description |
  |---|---|---|
  | `client_id` | `example-corp-integration` | ID of registered integration application |
  | `scope` | `offline_access` | OAuth scopes requested |
  | `redirect_uri` | `https://example.com/oauth_callback` | Integration callback URL |

  "The `redirect_uri` value `urn:ietf:wg:oauth:2.0:oob` indicates a serverless integration … the `authorization_code` is presented directly to the user in the browser." [DOC] **Caveat:** this text looks like it dates from the earlier Keycloak setup (see 2.2). Auth0 does not normally support the OOB redirect. Treat it as unverified and do not design around it.
- **Scopes:** [DOC #introduction-security-oauth-scopes]

  | Scope | Default | Description |
  |---|---|---|
  | `offline_access` | no | "Allows integration point to store refresh token after user is logged out. Offline tokens expires after 30 days inactivity, while regular `refresh_tokens` expire after 30 minutes." |

  No other API scopes are documented. Room Exports refers to token permissions `create:jobs:room-archive` [DOC #jobs-list-of-supported-jobs] and `jobs:room-exports` [DOC #jobs-room-exports]. The two names are inconsistent and are irrelevant to sync.
- **Authorizing requests:** `Authorization: Bearer {access_token}` on every request. [DOC #introduction-security-authorizing-requests]

Not documented by Interaxo: the authorization-code flow request format, PKCE, the authorize URL, device code, access-token lifetime, refresh-token rotation, MFA, logout, and whether public (secretless) clients are issued.

### 2.2 Identity provider facts [OIDC], [INFERRED]

The discovery document shows the IdP is **Auth0** on a custom domain. The `http://auth0.com/oauth/grant-type/...` grants confirm this. The web app also loads `auth0-spa-js` with `domain: 'login.collaboration-sso.com'` [INFERRED]. The `/auth/` prefix and the "offline tokens" wording in the docs are Keycloak conventions. The web app still contains an unused `keycloak` provider branch [INFERRED], so the platform probably migrated from Keycloak to Auth0. Parts of the auth docs may predate that migration.

Key discovery values [OIDC]:

| Field | Value |
|---|---|
| `issuer` | `https://login.collaboration-sso.com/` |
| `authorization_endpoint` | `https://login.collaboration-sso.com/authorize` |
| `token_endpoint` | `https://login.collaboration-sso.com/oauth/token` |
| `device_authorization_endpoint` | `https://login.collaboration-sso.com/oauth/device/code` |
| `userinfo_endpoint` | `https://login.collaboration-sso.com/userinfo` |
| `revocation_endpoint` | `https://login.collaboration-sso.com/oauth/revoke` |
| `mfa_challenge_endpoint` | `https://login.collaboration-sso.com/mfa/challenge` |
| `jwks_uri` | `https://login.collaboration-sso.com/.well-known/jwks.json` |
| `registration_endpoint` | `https://login.collaboration-sso.com/oidc/register` (Auth0 Dynamic Client Registration. Not mentioned by Interaxo. Do not rely on it, see 2.9) |
| `end_session_endpoint` | not advertised |
| `scopes_supported` | `openid profile offline_access name given_name family_name nickname email email_verified picture created_at identities phone address` |
| `response_types_supported` | `code`, `token`, `id_token`, and combinations |
| `response_modes_supported` | `query fragment form_post` |
| `code_challenge_methods_supported` | `S256`, `plain` (Auth0 docs say only S256 is accepted for PKCE. Use S256) |
| `token_endpoint_auth_methods_supported` | `client_secret_basic client_secret_post private_key_jwt none` (`none` = public client is possible at tenant level) |
| `grant_types_supported` | `client_credentials authorization_code refresh_token password implicit urn:ietf:params:oauth:grant-type:device_code urn:ietf:params:oauth:grant-type:token-exchange …password-realm …passwordless/otp …mfa-oob …mfa-otp …mfa-recovery-code urn:ietf:params:oauth:grant-type:jwt-bearer` |
| `dpop_signing_alg_values_supported` | `ES256` |

Tenant-level support for a grant does not mean a given client has it enabled. Interaxo configures grants per application.

[INFERRED] The first-party web app authorizes with `audience=https://api.interaxo.com/v1/`, `scope='openid profile email ix:web-api:all'`, `redirect_uri=<origin>/callback` and connection `Username-Password-Authentication`. `ix:web-api:all` is an internal scope for the web app's own `/api/`. It is not documented for the public API. **Do not reuse the web app's client id.** BYO means a separate registered application.

### 2.3 Recommended flow for a desktop app: Authorization Code + PKCE

The request shapes below are standard Auth0 behaviour [AUTH0] (https://auth0.com/docs/get-started/authentication-and-authorization-flow/authorization-code-flow-with-pkce/call-your-api-using-the-authorization-code-flow-with-pkce). The endpoints and audience are Interaxo-specific [DOC]/[OIDC].

**Step 1. Open the system browser** (not an embedded WebView):
```
GET https://login.collaboration-sso.com/authorize
  ?response_type=code
  &client_id={BYO_CLIENT_ID}
  &redirect_uri=http%3A%2F%2F127.0.0.1%3A{PORT}%2Fcallback
  &scope=openid%20profile%20email%20offline_access
  &audience=https%3A%2F%2Fapi.interaxo.com%2Fv1%2F
  &state={random}
  &code_challenge={BASE64URL(SHA256(code_verifier))}
  &code_challenge_method=S256
```
- `audience` is required. Auth0 must receive the API identifier to issue an access token for the custom API [AUTH0]. Interaxo docs give this same audience value [DOC], and the web app sends it [INFERRED].
- `offline_access` is required to get a refresh token [DOC][AUTH0]. Auth0 also requires "Allow Offline Access" on the API definition, which is Interaxo's setting.
- Optional Auth0 parameters: `prompt=login`, `login_hint={email}`, `connection={name}` (forces a specific Auth0 connection, e.g. an enterprise SSO connection). Interaxo does not document connection names.
- Success: `302` to `redirect_uri?code=...&state=...`.

**Step 2. Exchange the code:**
```
POST https://login.collaboration-sso.com/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=authorization_code
&client_id={BYO_CLIENT_ID}
&code_verifier={code_verifier}
&code={code}
&redirect_uri=http%3A%2F%2F127.0.0.1%3A{PORT}%2Fcallback
[&client_secret={secret}]   <- only if Interaxo registered a confidential client (see 2.9)
```
Response 200 [AUTH0]:
```json
{
  "access_token": "eyJ...",
  "refresh_token": "v1....",
  "id_token": "eyJ...",
  "token_type": "Bearer",
  "expires_in": 86400,
  "scope": "openid profile email offline_access"
}
```
- `refresh_token` is present only if `offline_access` was requested and allowed.
- `expires_in` is configured by Interaxo. The access-token lifetime is [UNDOCUMENTED], so always honour `expires_in`.

**Step 3. Refresh:**
```
POST https://login.collaboration-sso.com/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token&client_id={BYO_CLIENT_ID}&refresh_token={rt}[&client_secret=...]
→ 200 {"access_token", "expires_in", "scope", "token_type", "id_token"?, "refresh_token"? }
```
- If Interaxo enables Auth0 refresh-token rotation, every refresh returns a new `refresh_token` and the old one becomes invalid after a reuse interval. [AUTH0] Whether rotation is on is [UNDOCUMENTED]. Always persist the newest `refresh_token` returned, atomically, and serialize refreshes across threads.

**Step 4. Sign-out:** revoke the refresh token with `POST https://login.collaboration-sso.com/oauth/revoke` and form fields `client_id`, `token` (and `client_secret` if confidential). [OIDC][AUTH0] No end-session endpoint is advertised.

### 2.4 Redirect URI rules for loopback

- Interaxo documents only an `https://` example and the OOB URN. [DOC #introduction-security-oauth-login-parameters] Rules for `http://localhost` are [UNDOCUMENTED] by Interaxo.
- Auth0 behaviour [AUTH0]:
  - `http://localhost` and `http://127.0.0.1` callback URLs are accepted in "Allowed Callback URLs" (https://auth0.com/docs/get-started/applications/work-with-auth0-locally).
  - **Wildcard or any-port loopback is not supported. The port must be registered exactly.** An Auth0 staff answer from 2020 says "the port has to be explicitly configured" (https://community.auth0.com/t/are-port-wildcards-supported-pkce-for-desktop-apps/34132). This deviates from RFC 8252 §7.3.
  - The `redirect_uri` must match exactly, including path, between the authorize request, the token request and the registration.
  - Custom URI schemes (e.g. `com.example.synka://callback`) are allowed for Native applications.
  - For localhost callbacks Auth0 always shows the consent dialog, because localhost cannot be verified as first-party (https://support.auth0.com/center/s/article/Security-Risks-of-Using-Localhost-for-Callback-URL). Expect a consent screen on first login.
- **Implication for BYO config:** the app config needs `client_id` and also the exact `redirect_uri`, or at least a fixed port. Ask Interaxo to register two or three fixed loopback URIs, e.g. `http://127.0.0.1:53682/callback`, `http://127.0.0.1:53683/callback` and `http://localhost:53682/callback`, as fallbacks for port conflicts. A custom-scheme URI is an alternative.

### 2.5 Refresh tokens and lifetimes

| Item | Value | Source |
|---|---|---|
| Refresh token requires | `offline_access` scope | [DOC #introduction-security-oauth-scopes] |
| Offline refresh token expiry | "30 days inactivity" | [DOC]. Possibly stale Keycloak text. Verify |
| "Regular" refresh token expiry | "30 minutes" | [DOC]. Keycloak "SSO session" semantics. Probably not meaningful on Auth0 |
| Access-token lifetime | not documented, use `expires_in` | [UNDOCUMENTED] |
| Rotation / reuse detection | not documented | [UNDOCUMENTED] |
| Web app session timeout | `SESSION_TIMEOUT = 12` (unit unknown, probably hours) | [INFERRED], not applicable to API clients |

Design so that an idle machine longer than the refresh-token inactivity window (assume 30 days) needs an interactive re-login, and show a clear "sign in again" state.

### 2.6 Device Authorization (device code) flow

- Interaxo docs: not mentioned. [UNDOCUMENTED]
- The tenant supports it: `device_authorization_endpoint = https://login.collaboration-sso.com/oauth/device/code`. [OIDC]
- Auth0 prerequisites [AUTH0] (https://auth0.com/docs/get-started/authentication-and-authorization-flow/device-authorization-flow): Native application type, Device Code grant enabled (plus Refresh Token grant), **token endpoint auth method None (public client only)**, OIDC-conformant, and not created via Dynamic Client Registration.
- Request: `POST /oauth/device/code` (form) with `client_id`, `audience=https://api.interaxo.com/v1/` and `scope=openid profile email offline_access`. The response has `device_code`, `user_code`, `verification_uri`, `verification_uri_complete`, `expires_in` and `interval`.
- Poll: `POST /oauth/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `device_code` and `client_id`. Errors: `authorization_pending` (403), `slow_down` (429), `expired_token` (403), `access_denied` (403).
- Usable only if Interaxo enables it on the BYO client. It is a useful fallback when loopback ports are blocked.

### 2.7 Client credentials

- This is the only flow fully exemplified [DOC #introduction-security-authentication]. It yields a token for the client itself, not a user. The docs also say all API access uses "the identity of the user", so what a client-credentials token can see is [UNDOCUMENTED] (probably a service account configured by Interaxo).
- **Not suitable** for per-user desktop sync, and it requires shipping a secret.

### 2.8 Other mechanisms

| Mechanism | Status |
|---|---|
| API keys / personal access tokens | Not documented. Only Bearer tokens exist. [DOC] |
| HTTP Basic auth to the API | Not documented. The API returns `WWW-Authenticate: Bearer` on 401. [OBSERVED] |
| Resource-owner password grant | Tenant lists `password` [OIDC]. Not documented by Interaxo and bypasses MFA/SSO. Do not use |
| MFA | Not documented by Interaxo. Tenant has `mfa_challenge_endpoint` and MFA grants [OIDC]. With the browser-based auth-code flow, MFA and enterprise SSO are handled on the hosted login page, so the client needs nothing special |
| OIDC userinfo | `GET https://login.collaboration-sso.com/userinfo` with `Authorization: Bearer` (needs `openid` scope). Standard claims such as `sub`, `email`, `name` [OIDC]. Not Interaxo-documented |
| DPoP | Tenant advertises ES256 DPoP [OIDC]. Not documented for the API. Do not use |

### 2.9 Client registration for BYO: what the organisation must request

Per [DOC #introduction-security-enablement], the organisation contacts Interaxo support, and Interaxo registers the application in its Auth0 tenant. Nobody but Interaxo can create clients there. The public `registration_endpoint` [OIDC] is Auth0 Dynamic Client Registration. It is normally disabled, it creates third-party apps, and it is incompatible with device flow, so do not build on it. I did not probe it, because that would create a client.

Checklist for the organisation to send to Interaxo support:
1. Application type **Native** (public client), **Token Endpoint Authentication Method = None**, so no client secret is embedded in the desktop app. The docs say a `client_secret` is issued, so ask explicitly for a secretless client. If Interaxo only issues confidential clients, the app must store a secret, which is not truly secret on a desktop. BYO config should have an optional `client_secret` field for this case.
2. Grants: **Authorization Code** (PKCE S256), **Refresh Token**, and optionally **Device Code**.
3. Allowed Callback URLs: exact loopback URIs with fixed ports (see 2.4), and/or a custom scheme.
4. API access to audience `https://api.interaxo.com/v1/` with **Allow Offline Access** (refresh tokens).
5. Ask for confirmation of: refresh-token rotation on/off, refresh-token inactivity and absolute lifetimes, access-token lifetime, whether the OOB redirect works, and which Auth0 connections (password, enterprise SSO) are enabled for the client.
6. Ask whether API usage is billed or limited per client (the docs state Room Exports is "charged separately" [DOC #jobs-room-exports], so per-client metering exists).

BYO config fields the app should accept: `client_id`, optional `client_secret`, `redirect_uri` (or port list), `environment` (`com` / `se` / auto), optional `connection`, and optional extra `scope`.

### 2.10 Calling the API

```
GET https://api.interaxo.com/v1/{community}/rooms
Authorization: Bearer {access_token}
```
- Without a token or with an invalid one: `HTTP 401`, `WWW-Authenticate: Bearer`, **empty body** (`Content-Length: 0`) [OBSERVED on both hosts]. This contradicts "A JSON error object is returned by all errors" [DOC #introduction-error-handling], so handle empty error bodies.
- Response headers include Spring Security defaults (`Cache-Control: no-cache, no-store…`, `X-Frame-Options: DENY`, `Strict-Transport-Security`). [OBSERVED]

---

## 3. Current user / profile

### 3.1 Retrieve profile [DOC #users-and-groups-retrieve-a-profile]
```
GET {API_BASE}/{community_id}/people/{person_id}
GET {API_BASE}/{community_id}/people/-me-          <- currently authenticated user
```
Response 200, Person object [DOC #users-and-groups-the-person-object]:
```json
{ "id": "user@example.com", "email": "user@example.com", "name": "Example User", "locale": "nb-NO" }
```
| Key | Type | Description |
|---|---|---|
| `id` | string | User id |
| `name` | string | Name |
| `email` | string | Profile email |
| `locale` | string | Profile locale |

- This is community-scoped, so you need a community id first (`GET /v1/communities`). Before that, use OIDC `/userinfo` (2.8).
- "while the `id` property is typically an email address, it is not bound to the user profile email address. Integrators should not assume that the `id` is in the format of an email address." [DOC #users-and-groups-the-user-object]. Use `id` as the stable key and `email` for display.

### 3.2 Groups for a person [DOC #users-and-groups-retrieve-groups-for-a-person]
```
GET {API_BASE}/{community_id}/people/-me-/groups
→ [ { "id": "ae11b8a55", "type": "group", "name": "All Participants" }, ... ]
```
Group ids are max 100 characters and unique within a community. [DOC #users-and-groups-the-group-object]

### 3.3 Authority objects embedded in content [DOC #users-and-groups]
- User: `{ "id": "user@example.com", "type": "user" }`. Some responses also include `"name"`.
- Group: `{ "id": "ed11b8a55", "type": "group", "name": "All Participants" }`.

---

## 4. Projects hierarchy: communities → workspaces → rooms

```
Environment host (.com / .se)
 └─ Community   (customer area; slug id)          GET /v1/communities
     ├─ Workspace (grouping of rooms)              GET /v1/{community}/workspaces
     └─ Room      (project / org unit; slug id)    GET /v1/{community}/rooms[?workspace=]
         └─ content tree, root = room.content.root_id
```
"A community in Interaxo maps to a single customer area. Within a community a customer may have a number of rooms, which are logical containers typically used for projects or organisational units. Users of Interaxo may have access to more than one community." [DOC #communities-and-rooms]

**The sync unit is a room.** A sync root should be identified by `(api_host, community_id, room_id)`, with `root_id` as the tree root.

### 4.1 List communities [DOC #communities-and-rooms-list-all-communities]
```
GET {API_BASE}/communities            (supports limit/skip/sort)
→ 200 [ { "id": "example-corp", "name": "Example Corp - Sample Project" }, ... ]
```
| Key | Type | Description |
|---|---|---|
| `id` | string | Community identifier |
| `name` | string | Community name |

### 4.2 List workspaces [DOC #communities-and-rooms-list-all-workspaces]
```
GET {API_BASE}/{community_id}/workspaces    (supports limit/skip/sort)
→ 200 [ { "id": "my-workspace", "name": "My Workspace" }, ... ]
```

### 4.3 List rooms [DOC #communities-and-rooms-list-all-rooms]
```
GET {API_BASE}/{community_id}/rooms
    ?workspace={workspace_id}         optional: filter by workspace
    &{metadata_key}={value}           optional: filter by room metadata, e.g. project_ref=My Value
    &limit=&skip=&sort=
→ 200 [ Room, ... ]
```
Whether the list is limited to rooms the user is a member of, or includes all community rooms, is [UNDOCUMENTED]. The general rule "only data the user has read access to" [DOC #introduction-security] suggests membership filtering.

### 4.4 Retrieve a room [DOC #communities-and-rooms-retrieve-a-room]
```
GET {API_BASE}/{community_id}/rooms/{room_id}
```
Room object [DOC #communities-and-rooms-the-room-object]:
```json
{
  "id": "my-room",
  "name": "My room",
  "description": "My description",
  "content": { "root_id": "a2afd701-ade2-4f94-94f9-1d5dbab69622" },
  "workspace": { "id": "my-workspace", "name": "My workspace" },
  "metadata": {
    "my_key": { "label": "My label", "value": "Some value" }
  }
}
```
| Key | Type | Description |
|---|---|---|
| `id` | string | Room id (slug) |
| `name` | string | Display name |
| `description` | string | Description |
| `content.root_id` | string | UUID of the root content folder |
| `workspace.id` / `.name` | string | Workspace relation |
| `metadata` | map key → `{label, value}` | Room metadata |

### 4.5 Lock / unlock a room [DOC #communities-and-rooms-locking-rooms]
```
PATCH {API_BASE}/{community_id}/rooms/{room_id}
Content-Type: application/json
{ "lock": false }
→ 200 { "id": "my-room", "lock": "false", ... }
```
This is a whole-room administrative lock, not file check-out. The `lock` field is not in the Room object table, and the example returns it as the string `"false"`, so its type is ambiguous. Required permissions are [UNDOCUMENTED]. A sync client should probably only read it, and should treat a locked room as read-only. Whether `GET` returns `lock` is [UNDOCUMENTED].

---

## 5. Documents: folder tree, listing, metadata, change detection

### 5.1 Content item types [DOC #room-content-content-objects]

| Name | `type` | Category | Allowed children |
|---|---|---|---|
| Simple Folder | `simple-folder` | Folder | Folder, File, Link |
| Active Folder | `active-folder` | Folder | Entry |
| Active Folder with Workflow | `workflow-folder` | Folder | Entry |
| Entry | `entry` | Entry | File |
| File | `file` | File | none |
| Link (internal) | `internal-link` | Link | none |
| Link (external) | `external-link` | Link | none |

Mapping for a file-sync client (design suggestion, not from the docs):
- `simple-folder` → local directory, `file` → local file. This is the core sync.
- `active-folder` / `workflow-folder` → metadata registers ("case folders", "issue trackers" [DOC #room-content-active-folders]). Their children are `entry` items. Each entry holds file attachments. Options: skip them, or expose them read-only as `<folder>/<entry title>/<attachments>`. Entries can only be created through the API with field metadata, so uploading into them from Explorer does not map cleanly.
- Links: skip, or render as `.url` shortcut files.

### 5.2 Common properties of all content items [DOC #room-content-content-objects]

```json
{
  "type" : "simple-folder",
  "id" : "8be54905-6c9d-430d-9e3f-b40006fdba3f",
  "parent_id" : "ec4e334d-74e8-4ea4-8f28-4da942a3b80c",
  "name" : "Simple Folder",
  "path" : "/Collaborate/All Types/Simple Folder",
  "created" : "2017-01-12T10:24:41.739Z",
  "last_modified" : "2017-01-12T11:06:44.101Z",
  "created_by" : { "id" : "user@example.com" },
  "last_modified_by" : { "id" : "user@example.com" },
  "owner" : { "id" : "user@example.com" },
  "lock_info" : { "owner": { "id": "user@example.com" }, "timestamp": "2017-01-12T11:06:44.101Z" },
  "child_count": 0
}
```
| Key | Type | Description |
|---|---|---|
| `type` | string | Item type (5.1) |
| `id` | string | UUID |
| `parent_id` | string | UUID of parent |
| `name` | string | Display name |
| `path` | string | "Path relative to room, like `/My Folder/My File.jpg`" |
| `created` | string | ISO 8601 creation datetime (ms precision, `Z`) |
| `last_modified` | string | ISO 8601 last-modified datetime |
| `created_by` | object | User |
| `last_modified_by` | object | User |
| `owner` | object | User or Group |
| `lock_info` | object | Optional, present only if locked: `{owner: User, timestamp: ISO8601}` [DOC #room-content-content-objects-response-the-lock-info-object] |

Additional properties per type:
- `simple-folder`: `child_count` (number of immediate children). [DOC #room-content-simple-folders-the-simple-folder-object]
- `active-folder`: `child_count`, `fields` (field definitions), `features` (e.g. `main-document`, `revisioning`). [DOC #room-content-active-folders-the-active-folder-object]
- `workflow-folder`: as active-folder plus `steps`. [DOC #room-content-active-folders-with-workflow-the-workflow-folder-object]
- `entry`: `child_count`, `fields`, `step`. [DOC #room-content-entries-the-entry-object]
- `file`: see 5.3.
- `internal-link`: `target` (`_blank`/`_self`), `destination` (UUID). `external-link`: `target`, `url`. [DOC #room-content-links]

### 5.3 File object [DOC #room-content-files-the-file-object]

| Key | Type | Description |
|---|---|---|
| `version` | string | Version label, e.g. `"1.0"`, `"2.0"` (major.minor; see upload `major` flag) |
| `mime_type` | string | e.g. `image/png` |
| `file_size` | number | Size in bytes |
| `content_url` | string | "File content public URL", e.g. `https://ix.interaxo.com/v1/community/public/v2/files/id/content?t=...` |

Seen in some responses but not in the table:
- `original_name` (string) and `main_document` (boolean), in the create-file response [DOC #room-content-files-create-a-file].
- `content_url` appears in the create, update and search-by-name examples, but **not** in the list-children or full-text-search examples [DOC #room-content-content-objects-list-children]. Do not assume it is always present.

**Not available:** checksum/hash (MD5/SHA), ETag, numeric version id, version history listing, last-modified-by name. None are documented. The best available change key per file is `(id, version, last_modified, file_size)`. Whether HTTP `ETag`/`Last-Modified` headers are returned is [UNDOCUMENTED].

### 5.4 Retrieve one item [DOC #room-content-content-objects-retrieve-a-content-item]
```
GET {API_BASE}/{community}/rooms/{room}/content/{id}
→ 200 <content object of any type>     (file: metadata only)
```

### 5.5 List children (folder listing) [DOC #room-content-content-objects-list-children]
```
GET {API_BASE}/{community}/rooms/{room}/content/{id}/children?limit=&skip=&sort=
→ 200 [ <content object>, ... ]      (bare JSON array)
```
Example (`?sort=id&limit=2`):
```json
[
  { "type": "simple-folder", "id": "8be54905-...", "parent_id": "ec4e334d-...", "name": "Simple Folder",
    "path": "/Collaborate/All Types/Simple Folder", "created": "...", "last_modified": "...",
    "created_by": {"id": "user@example.com"}, "last_modified_by": {"id": "user@example.com"},
    "owner": {"id": "user@example.com"}, "child_count": 0 },
  { "type": "file", "id": "c2afd701-...", "parent_id": "ec4e334d-...", "name": "File.txt",
    "path": "/Collaborate/All Types/File.txt", "created": "...", "last_modified": "...",
    "created_by": {...}, "last_modified_by": {...}, "owner": {...},
    "version": "1.0", "mime_type": "text/plain", "file_size": 7 }
]
```
- Full tree walk: start at `room.content.root_id`, then recursively list children of every folder.
- No recursive or "list all descendants" endpoint is documented.
- `child_count` lets you skip empty folders.
- Whether a folder's `last_modified` changes when descendants change is [UNDOCUMENTED]. Do not prune subtree scans on it without testing.

### 5.6 Search (scoped to a folder subtree) [DOC #room-content-content-object-search]
```
GET {API_BASE}/{community}/rooms/{room}/content/{id}/search
    ?type=file                       required; one or more of: file, folder, active-folder, workflow-folder, entry
    &name=File.jpg                   field=value criteria
    &folder_id=...&folder_id=...     optional: scope to multiple folders
    &skip=&limit=&sort=
→ 200 [ <content object>, ... ]
```
- The search type for simple folders is `folder`, not `simple-folder`.
- Searchable properties: `name`, `created` (`yyyy-mm-dd` or `yyyy-mm-dd'T'HH:mm:ssZ`), `version` (label), `content` (full text), `instruction` (folder instruction). Entries can also be searched on their own fields. [DOC #room-content-content-object-search-list-children-search-criterias]
- Range queries `field_from=` / `field_to=` work for numeric and date fields [DOC #room-content-content-object-search-range-queries]. Whether `created_from`/`created_to` works for files is [UNDOCUMENTED].
- Repeat a parameter to OR values. Partial match uses `*`.
- **There is no `last_modified` criterion, so no "modified since" search exists.**
- "These results are 'eventually consistent'". Renames are not reflected immediately. [DOC #room-content-content-object-search-partial-matching-important-note]

### 5.7 Find files by name within a folder [DOC #room-content-files-search-for-a-file-in-folder-by-file-name]
```
GET {API_BASE}/{community}/rooms/{room}/content/{id}/files?name=My File
→ 200 [ <file>, ... ]
```
The docs describe the same endpoint twice: once matching the file name, once matching files "located within a folder matching the name" [DOC #room-content-files-search-for-a-file-in-a-folder-by-folder-name]. Its semantics are ambiguous. It is also eventually consistent. Useful at most to check for an existing name before upload.

### 5.8 Change detection

| Mechanism | Status |
|---|---|
| Delta / changes feed with cursor | **Not available** [UNDOCUMENTED] |
| `modified_since` filter on listing | **Not available** |
| Webhooks / push events | **Not documented.** The Room Exports example has a stray `HTTP/1.1 201 CREATEDnotify_webhook` [DOC #jobs-room-exports], which hints at an undocumented job webhook parameter. Not usable for content changes |
| Activity feed (polling) | **Available**, see below |
| Full rescan | Always works: walk the tree and diff `(id, parent_id, name, version, last_modified, file_size)` |

**Activity feed** [DOC #activities]
```
GET {API_BASE}/{community_id}/activities?roomId={room_id}&scope=ALL&skip=0&limit=50
→ 200
{
  "items": [
    {
      "id": 3990916446,
      "type": "entry-updated",
      "user": { "type": "user", "id": "user@example.com", "name": "Example User" },
      "date": "2026-09-28T07:40:01.060Z",
      "summary": {
        "id": "3ab40c92-825f-4963-aab0-32552d4696b2",
        "name": "Example document",
        "parent_id": "6c9d9051-2f4e-4b8a-9c1d-7e5f3a2b1c0d",
        "parent_path": "/Public/Documents",
        "room_id": "my-room",
        "room_name": "My Room",
        "community_id": "my-community",
        "workspace_id": "my-workspace",
        "workspace_title": "My Workspace"
      }
    }
  ],
  "has_more": false
}
```
| Field | Type | Notes |
|---|---|---|
| `items[].id` | number | Activity id. Whether ids are monotonic is [UNDOCUMENTED] |
| `items[].type` | string | Examples: `entry-updated`, `folder-deleted`. **The full list of types is undocumented**. Whether file create, update, rename, move and delete each produce an activity is [UNDOCUMENTED] |
| `items[].user` | object | Authority |
| `items[].date` | string | ISO 8601 |
| `items[].summary` | object | `id`, `name`, `parent_id` (omitted in the deleted-folder example), `parent_path`, `room_id`, `room_name`, `community_id`, `workspace_id`, `workspace_title` (may be omitted) |
| `has_more` | boolean | More pages. Increase `skip` |

- Order: most recent first. Query parameters: `roomId` (optional), `scope` (optional; only `ALL` is shown and other values are undocumented), `skip`, `limit`. There is **no `since` parameter**.
- The endpoint line is written as "``GET /activities(community context)`" (typo). The curl example shows the actual path `{API_BASE}/{community}/activities`.
- Suggested polling: page from `skip=0` until you reach an activity with `id` (or `date`) at or before the last processed one. Collect the affected `summary.id` / `parent_id` and re-fetch those items and their parent folders. Fall back to a periodic full rescan, because coverage of file events is unverified and the activity window or retention is undocumented.

---

## 6. Download

### 6.1 File content [DOC #room-content-files-download-a-file]
```
GET {API_BASE}/{community}/rooms/{room}/files/{id}/content
    [?version=2.0 | ?v=2.0]           optional; no version = latest
Authorization: Bearer ...
→ 200 <bytes>
   Content-Type: image/jpeg;charset=UTF-8   (only header documented)
```
- Path is `/files/{id}/content`, not `/content/{id}/...`.
- `Content-Length`, `Content-Disposition`, `ETag`: [UNDOCUMENTED].
- **Range requests for file content: [UNDOCUMENTED].** Range support (`Accept-Ranges: bytes`, `206 Partial Content`) is documented only for job content [DOC #jobs-download-job-content]. Test with `Range: bytes=0-0` before relying on resumable downloads. Fall back to full re-download.
- Download older versions with `?version=`. No endpoint lists the available versions. [UNDOCUMENTED]
- Files attached to entries use the same endpoint (they are `file` items).

### 6.2 Pre-signed / public URL
- `content_url` on file objects is a "File content public URL" on the **web** host (`https://ix.interaxo.com/v1/{community}/public/v2/files/{id}/content?t=...`). [DOC #room-content-files-the-file-object]
- Without `t`, that path returns 401. [OBSERVED]
- Lifetime, scope and whether the URL is tied to a version: [UNDOCUMENTED]. Prefer the authenticated API endpoint. `content_url` might be useful for handing a download to another process without a bearer token, once its validity has been tested.

### 6.3 Thumbnail and preview [DOC #room-content-files-download-file-thumbnail], [DOC #room-content-files-download-file-preview]
```
GET {API_BASE}/{community}/rooms/{room}/files/{id}/thumbnail   → image/png, max 100px on the long side
GET {API_BASE}/{community}/rooms/{room}/files/{id}/preview     → image/png, max 960px on the long side
```
These could back Explorer thumbnails for placeholder (cloud-only) files.

---

## 7. Upload

### 7.1 New file [DOC #room-content-files-create-a-file]
```
POST {API_BASE}/{community}/rooms/{room}/content/{parent_id}/children
Authorization: Bearer ...
Content-Type: multipart/form-data; boundary=...

--boundary
Content-Disposition: form-data; name="file"; filename="abc.txt"
Content-Type: text/plain

<bytes>
--boundary--
```
(curl: `curl --form file=@abc.txt ".../content/$ID/children"`)

Response `200 OK` (not 201):
```json
{
  "type" : "file",
  "id" : "3ab40c92-825f-4963-aab0-32552d4696b6",
  "parent_id" : "ed507f9c-8e33-4616-85be-22038cd3a3a8",
  "name" : "abc.txt",
  "original_name" : "abc.txt",
  "path" : "/Collaborate/Upload/Simple Folder/abc.txt",
  "content_url" : "https://ix.interaxo.com/v1/community/public/v2/files/id/content?t=...",
  "created" : "2017-03-16T22:19:08.281Z",
  "last_modified" : "2017-03-16T22:19:08.281Z",
  "created_by" : { "id" : "user@example.com" },
  "last_modified_by" : { "id" : "user@example.com" },
  "owner" : { "id" : "user@example.com" },
  "version" : "1.0",
  "mime_type" : "text/plain",
  "file_size" : 18,
  "main_document": true
}
```
- `parent_id` must be a `simple-folder` or an `entry`. [DOC]
- Request body: only `file` (multipart part, required). There are no other metadata fields. The name comes from the multipart `filename`. [DOC #room-content-files-create-a-file-request-body]
- **Filename rules:** must not start with `.`. Must not contain `<` `>` `"` `'` `\` `|` `/` `*` `?` `:` `;` ("including", so the list may be incomplete). [DOC] The `'` and `;` restrictions are stricter than Windows. Local files with these names need a conflict or "cannot sync" state. Dot-files such as `.gitignore` and Office lock files `~$...` (the latter allowed) need explicit handling.
- Conflict behaviour when a file with the same name already exists: [UNDOCUMENTED]. Interaxo may reject the upload, auto-rename it, or add a version. The presence of `original_name` suggests auto-renaming may happen. Check `name` in the response against what you sent.
- Max size, timeouts, chunked/resumable upload, pre-signed or S3 upload: **[UNDOCUMENTED / not available in the public API]**. Upload is a single multipart POST.
  - [INFERRED] The web app uses chunked Uppy uploads (4 MB chunks, 4 parallel, max `250gb`) against its internal `/api/`. That is not the public API and must not be used. It does show the platform handles very large files, but the public multipart limit is unknown. Ask Interaxo.
- There is an inconsistency: "Create and update content items" says "only Entries can be created and updated" [DOC #room-content-content-objects-create-and-update-content-items], while the Files section documents file creation. The Files section is more specific and current. Treat file upload as supported.
- `main_document: true` in the example concerns entries with the main-document feature [DOC #room-content-main-document]. It is irrelevant for simple folders.

### 7.2 New version of an existing file [DOC #room-content-files-update-a-file]
```
POST {API_BASE}/{community}/rooms/{room}/files/{id}/versions[?major=true|false]
Authorization: Bearer ...
Content-Type: multipart/form-data   (part name "file")
→ 200 <file object>   e.g. "version": "2.0", updated last_modified / file_size
```
- `major`: optional boolean. `true` = major version (default), `false` = minor. Major is presumably `1.0 → 2.0` and minor `1.0 → 1.1` [UNDOCUMENTED exact numbering].
- Whether a different multipart `filename` renames the item: [UNDOCUMENTED]. Send the current server name.
- Optimistic concurrency (If-Match or a base-version parameter): **not available**. To detect conflicts, re-`GET` the item before uploading and compare `version` and `last_modified` with the last synced state. A race window remains.
- Behaviour when the file is locked by another user (`lock_info` present): [UNDOCUMENTED]. Probably `403` or `422`.

### 7.3 Upload sequence for the sync engine (suggested)
1. Local new file → check name rules → `POST .../content/{parentFolderId}/children` → store the returned `id`, `version`, `last_modified`, `file_size` and the server `name`.
2. Local modified file → `GET .../content/{id}` → if server `version` equals the last synced version, `POST .../files/{id}/versions?major=true`. Otherwise it is a conflict (keep both: upload the local copy as a new file with a conflict-suffixed name).
3. Local new folder → **cannot create via the API** (see 8). Surface "folder creation not supported" or keep it local-only.

---

## 8. Create, rename, move, delete; trash and restore

| Operation | Files | Simple folders | Source |
|---|---|---|---|
| Create | Yes, multipart upload (7.1) | **Not supported**: "Creating folders is not supported in the current version of the API. Contact Support if this is important to you." | [DOC #room-content-simple-folders-create-a-simple-folder] |
| Rename | **Not supported.** No endpoint. "Creating and updating other content items [than entries] is not supported" | **Not supported**: "Updating folders is not supported" | [DOC #room-content-content-objects-create-and-update-content-items], [DOC #room-content-simple-folders-update-a-simple-folder] |
| Move | **Not supported.** No endpoint | **Not supported** | same |
| Copy | Not documented | Not documented | [UNDOCUMENTED] |
| Delete | `DELETE .../content/{id}` → `204 No Content` | same (applies to all content items) | [DOC #room-content-content-objects-delete-a-content-item], [DOC #room-content-files-delete-a-file], [DOC #room-content-simple-folders-delete-a-simple-folder] |
| Trash / recycle bin / restore | **Not documented.** Whether DELETE is soft (recoverable in the web UI) or permanent is unknown | | [UNDOCUMENTED] |

```
DELETE {API_BASE}/{community}/rooms/{room}/content/{id}
Authorization: Bearer ...
→ 204 No Content
```
- Deleting a folder presumably deletes its subtree. Whether it is recursive or fails on non-empty folders is [UNDOCUMENTED].
- Deletes are attributed to the API user in audit trails. [DOC #introduction-security]
- The only update endpoint for content is `PUT .../content/{id}` for **entries** (field values) [DOC #room-content-entries-update-an-entry]. Its body has only `type` and `fields`, with no `name` or `parent_id`, so it cannot rename or move.

**Implications for the sync client:**
- Local rename or move cannot be applied server-side. Options: (a) block or revert it with a notification; (b) emulate it as upload-new + delete-old, which loses version history, comments and the item id; (c) treat the folder tree as server-managed and read-only, allowing only file content changes and new files. (c) or (a) is recommended. (b) is destructive and should be opt-in.
- Local folder creation cannot be synced up.
- Local delete → `DELETE`. Because trash semantics are unknown, confirm with the user and prefer "delete locally only" by default.
- Server-side renames and moves (done in the web UI) show up on a rescan as the same `id` with a new `name`, `parent_id` or `path`. Track items by `id`, not path.

---

## 9. Locks, check-out/check-in, open-in-browser URLs

### 9.1 Item locks
- Read-only: `lock_info` `{owner, timestamp}` is present on any content item when it is locked. [DOC #room-content-content-objects-response-the-lock-info-object]
- **No endpoints to lock, unlock, check out or check in items.** [UNDOCUMENTED / not available]
- The only lock endpoint is the whole-room `PATCH .../rooms/{room} {"lock": true|false}` (4.5).
- Sync behaviour: if `lock_info.owner.id` is not the current user, mark the local file read-only, show the owner, and do not upload versions.

### 9.2 Web URLs for "open in browser"
- Web hosts: `https://ix.interaxo.com` and `https://ix.interaxo.se`. [DOC #introduction-environments]
- **Deep-link formats are not documented.** No `web_url` field is returned by the API.
- [INFERRED] from the web-app router (`Path.map` routes in `https://ix.interaxo.com/classic/app.js`):
  - Community overview: `https://ix.interaxo.{com|se}/{community_id}/overview`
  - Room document library: `https://ix.interaxo.{com|se}/{community_id}/{room_id}/documentLibrary`
  - Item (folder or file node): `https://ix.interaxo.{com|se}/{community_id}/{room_id}/documentLibrary/{nodeId}`. The router pattern is `/:tenant/:room/:tab(/:id)(/:page)(/:query)`, and the app builds item links as `"/" + communityId + "/" + roomId + "/documentLibrary/" + nodeId`.
  - Room search: `/{community_id}/{room_id}/{nodeId}/search/{phrase}`
  - That the web `nodeId` equals the API content `id` (UUID) is very likely but unverified. These links are internal and may change. Ask Interaxo for a supported permalink format.

---

## 10. Rate limits, errors, pagination

### 10.1 Rate limits
- Interaxo API: **[UNDOCUMENTED]**. No rate-limit headers were seen (only on 401 responses [OBSERVED]). Implement client-side throttling (e.g. 4–8 concurrent requests per host) and exponential backoff on `429` and `503`. `503 network_communication_error` is documented [DOC #introduction-error-handling-api-error-codes]. `429` is not documented, but handle it anyway, including `Retry-After`.
- Auth0 token endpoint: Auth0 applies its own rate limits. Refresh only when needed. [AUTH0]

### 10.2 Error format [DOC #introduction-error-handling]
```json
HTTP/1.1 500
Content-Type: application/json; charset=UTF-8
{ "trace_id": "AJI0WC", "code": "file_streaming_error", "message": "Can not stream file" }
```
Validation error:
```json
HTTP/1.1 400
{
  "trace_id": "AWCQN5",
  "code": "validation_error",
  "message": "Validation failed",
  "errors": [
    { "message": "Limit field must be positive number", "code": "list.invalid_limit_field" },
    { "message": "Invalid sort parameter", "code": "list.invalid_sort_field" }
  ]
}
```
| Key | Type | Description |
|---|---|---|
| `trace_id` | string | Unique trace id. Include it in support requests and log it |
| `code` | string (the table says `number`, but the examples are strings) | Error code |
| `message` | string | Description |
| `errors` | array of `{code, message}` | Validation details |

Error codes [DOC #introduction-error-handling-api-error-codes]:

| Code | HTTP | Meaning |
|---|---|---|
| `validation_error` | 400 | Invalid data sent |
| `unauthorized` | 401 | Invalid OAuth token (also: wrong environment host) |
| `forbidden` | 403 | Insufficient rights |
| `not_found` | 404 | Resource not found |
| `invalid_node_type` | 422 | Item type not valid for the resource or operation (e.g. uploading into an active folder, or `/files/{id}/content` on a folder) |
| `unexpected_error` | 500 | Internal error |
| `file_streaming_error` | 500 | File streaming I/O error |
| `network_communication_error` | 503 | Service unavailable |

[OBSERVED] Unauthenticated 401 responses have an **empty body**, not the JSON error object. Parse defensively.

### 10.3 Pagination, sorting, filtering [DOC #introduction-query-parameters]
All `GET` operations on collections accept:

| Param | Type | Example | Description |
|---|---|---|---|
| `limit` | number | `?limit=1000` | Max items |
| `skip` | number | `?skip=1000` | Offset |
| `sort` | string | `?sort=-name` | Sort field. A `-` prefix means descending |

- Collections return a **bare JSON array** with no total count, no `next` link and no `Link` header documented. Page until a page returns fewer than `limit` items.
- Exception: activities return `{ "items": [...], "has_more": bool }`. [DOC #activities-list-activities-response]
- Default `limit` and maximum `limit`: [UNDOCUMENTED]. The docs' own example uses `limit=1000`. The mentionable-authorities endpoint documents a default of 100. Always pass an explicit `limit` (e.g. 500) and a stable `sort` (e.g. `sort=id`) so offset paging is deterministic.
- Offset paging over a changing folder can skip or duplicate items. Reconcile by `id`.
- Sortable field names beyond `id` and `name` (seen in examples): [UNDOCUMENTED].

### 10.4 Conventions
- JSON with snake_case properties. Timestamps are ISO 8601 UTC with milliseconds (`2017-01-12T10:24:41.739Z`).
- Content ids are UUIDs. Community, room, workspace and group ids are strings.
- Query values must be URL-encoded (the docs' examples are shown unencoded "for readability" [DOC #room-content-content-object-search]).

---

## 11. Other documented endpoints (not needed for core sync)

For completeness (all under room context unless noted):
- Entries: create `POST .../content/{activeFolderId}/children` (JSON `{type:"entry", fields:[{id,type,value}], step?:{id}}`), update `PUT .../content/{entryId}` (`{type:"entry", fields:[...]}`), delete, list attachments, move between workflow steps `POST .../content/{id}/steps/{step_id}` with body `"<entry_id>"` → 204. The endpoint line says `.../content/{id}/{step_id}` but the curl example uses `/steps/`. Numeric fields are sent and received ×100. [DOC #room-content-entries], [DOC #room-content-active-folders-field-definitions-defining-number-fields]
- Main document of an entry: `GET|PATCH|DELETE .../content/{entryId}/document`. [DOC #room-content-main-document]
- Comments: `GET|POST .../content/{id}/comments`, `GET|PUT|DELETE .../content/{id}/comments/{comment_id}`, `?format=raw|html`, `?include=permissions`. Mentionable authorities: `GET .../content/{id}/comments/mentionable-authorities?type=&q=&limit=` (default limit 100). [DOC #room-content-comments]
- Room-level jobs (legacy): `GET .../jobs/{id}`, `POST .../jobs/content-exports` (CSV/Excel export of entries) → 201 + `Location`. Download the result via `/files/{jobId}/content`. Statuses: `PENDING`, `IN_PROGRESS`, `DONE`, `CANCELLED`. [DOC #room-content-room-level-jobs]
- Community jobs: `POST /{community}/jobs` (`type: room-archive`, `room_id`, `notify_email`, `parameters{format: csv|pdf, include_internal_comments, include_all_versions}`), `GET /{community}/jobs/{id}`, `DELETE /{community}/jobs/{id}` → 202, `GET /{community}/jobs/{id}/content` (supports `Range` → 206). Requires a special token permission, and is "charged separately". Statuses: `pending`, `in_progress`, `done`, `cancelled`, `failed`. [DOC #jobs]

---

## 12. Gaps, ambiguities and questions for Interaxo support

**Auth / BYO**
1. Will Interaxo register a **public Native** client (token auth `none`, PKCE) per customer organisation, or only confidential clients with secrets as the docs state?
2. Exact loopback redirect URIs: Auth0 requires exact ports with no wildcard. Will they register multiple fixed `http://127.0.0.1:{port}/callback` URIs and/or a custom scheme?
3. Is the Device Code grant available for such clients?
4. Refresh tokens: is rotation enabled? What are the real inactivity and absolute lifetimes? The "30 days / 30 minutes" text looks like Keycloak-era wording. What is the access-token lifetime?
5. Is `urn:ietf:wg:oauth:2.0:oob` still supported? Unlikely on Auth0.
6. Which API scopes or permissions, if any, beyond `offline_access`, should a sync client request? The web app uses the internal `ix:web-api:all`.
7. Is API use metered or billed per client or organisation?

**Environment**

8. How should a client find out whether a community lives on `.com` or `.se`? Does `/v1/communities` on one host list the other host's communities?

**Content / sync**

9. Is there any delta/changes endpoint, `modified_since` filter, or webhook for content changes? What is the full list of activity `type` values? Do file create, update, rename, move and delete all produce activities? What is the activity retention?
10. Is there any checksum/hash or ETag for file content? Any version-history listing endpoint?
11. Does `GET /files/{id}/content` support `Range` and return `Content-Length`?
12. What is the maximum upload size and request timeout for multipart upload? Is there a chunked/resumable upload in the public API?
13. What happens on name conflicts during upload (reject, auto-rename, new version)?
14. Are folder create, rename and move, and file rename and move, on the roadmap? The docs repeatedly say "Contact Support if this is important to you".
15. Is DELETE soft (recoverable from a trash in the web UI) or permanent? Is there a restore API?
16. Are there lock / check-out / check-in endpoints? What happens when uploading a version of a file locked by another user?
17. What is the validity of `content_url` (`?t=` token): lifetime, version-pinned or not?
18. Is there a supported permalink format for "open in browser" (the inferred `/{community}/{room}/documentLibrary/{id}`)?
19. What are the rate limits, and the default and maximum `limit` for collection endpoints?

**Doc inconsistencies noted**
- "Only Entries can be created and updated" vs the documented file upload and file versions endpoints.
- Error `code` typed as `number` but given as strings. The 401 body is empty in practice.
- Room `lock` is returned as the string `"false"` and is missing from the Room object table.
- The two "Search for a file…" sections share one endpoint with different descriptions.
- Move-entry path: `/content/{id}/{step_id}` vs `/content/{id}/steps/{step_id}`.
- Job permission named `create:jobs:room-archive` vs `jobs:room-exports`.
- Discovery URL path `/auth/.well-known/...` (Keycloak-style) now serves Auth0 metadata.
