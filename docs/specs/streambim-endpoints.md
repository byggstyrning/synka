# StreamBIM REST API: implementation spec for a file-sync client

**Source:** `github.com/jonatanjacobsson/streambim-api-docs` at commit `a07a177` (2026-09-24; `master` and `main` are identical). Citations look like `file:line` in that repo.

The repo calls itself **unofficial** documentation (`AGENTS.md:3`). Much of it was reverse-engineered from the StreamBIM web app; for example, one section is titled "Observed in HAR" (`api/attachments.md:900`). The official reference at `https://apidoc.streambim.com` (linked from `api/attachments.md:15`) returned HTTP 401 to an unauthenticated fetch, so I could not use it.

**Tags used below**

| Tag | Meaning |
|---|---|
| **[DOC]** | The docs say this. A citation follows. |
| **[LIVE]** | I saw this on 2026-10-20 with unauthenticated read-only GETs to public StreamBIM endpoints. Entries dated 2026-11-12 come from Synka's own sign-ins against `sweden`, read from its engine log. The docs don't say it. |
| **[INFERRED]** | My reading of the docs, examples or naming. Check it before relying on it. |
| **[UNDOCUMENTED]** | The docs say nothing. Test it against a live server or ask Rendra (`support@rendra.io` / `cto@rendra.io`, `README.md:83`, `AGENTS.md:66`). |
| **[GENERAL]** | General AWS, S3, Cognito or HTTP behavior. StreamBIM docs don't cover it. |
| **[WEBAPP]** | StreamBIM's own web app does this: its public bundle `app.streambim.com/assets/mgw-*.js` and the `mgw/config/environment` meta tag, read on 2026-11-12. |

---

## TL;DR: checking the summary "region selector + possible MFA + plain S3 + bring-your-own client id"

| Claim | Verdict |
|---|---|
| Region selector | **Confirmed.** `GET https://global.streambim.com/regions.json` needs no auth and returns region subdomains. Every API call then goes to `https://{subdomain}.streambim.com` [DOC `api/regions.md:9-11`]. Accounts and tokens appear to be per region, because each region has its own Cognito user pool [LIVE]. |
| Possible MFA challenge | **Confirmed.** `POST /auth/v1/login` can return `"result": "CHALLENGE_REQUESTED"`. The client then calls `POST /auth/v1/mfa/verify` with `username`, `session` and a 6-digit TOTP `code` [DOC `api/authentication.md:48`, `:218-251`]. |
| Upload is a normal S3 upload | **Mostly true, in 3 steps:** (1) `POST .../v2/upload-tickets` returns a presigned S3 URL, (2) a raw `PUT` to S3, (3) a JSON:API `POST` that registers the document or revision. Multipart upload for large files exists behind a `signer-url` (`.../aws/_uploadsigner`), but its protocol is **[UNDOCUMENTED]**. |
| Download is a normal S3 download | **Only partly.** You ask StreamBIM for a short-lived download link and GET it from the StreamBIM host. The docs don't say whether that GET redirects to S3 for documents; they do say it for attachments. |
| "Bring your own client id" | **The docs don't support this.** They describe no OAuth2/OIDC client registration for third parties, no authorization-code + PKCE flow, no device-code flow and no redirect-URI registration. Password login goes to StreamBIM's own server endpoint `/auth/v1/login`, which proxies to AWS Cognito, so a client needs **no client id at all**. The only client ids are StreamBIM's own per-region Cognito app clients [LIVE], and their `redirect_uri` is fixed to StreamBIM's server, so a third-party app can't use them. Users who can only sign in through SSO have no documented path for a native client. If "BYO client id" comes from the Interaxo side, it belongs to Interaxo, not StreamBIM. |

---

## 0. Common conventions

### 0.1 Base URL and path families

`https://{region}.streambim.com` [DOC `README.md:5-18`, `api/authentication.md:5-7`]

| Prefix | Purpose | Format | Source |
|---|---|---|---|
| `/auth/v1/*` | Login, refresh, MFA, federation | Request `application/json; charset=utf-8`, response `application/json` | `README.md:43` |
| `/mgw/api/v{2,3}/*` | Global "management gateway": projects, users, profiles | JSON:API, `application/vnd.api+json` | `README.md:44` |
| `/project-{projectId}/api/v1/v2/*` | Project-scoped JSON:API resources (documents, folders, revisions, upload tickets). The version really is doubled: `api/v1/v2`. | JSON:API | `README.md:45`, `api/documents.md:49` |
| `/project-{projectId}/api/v1/*` (no `v2`) | Project-scoped non-JSON:API helpers: `documents/{id}/downloadlink`, `microtoken`, `documents/_upload`, `documents/export/json/`, `documents/{id}/_show`, `sync`, `sse/*`, `batch` | Varies | per section below |
| `/cmv3/*` | Converter service for IFC conversion. Not needed for sync. | JSON:API | `README.md:46` |
| `/bcf/2.1/*` | BCF 2.1 | `application/json` | `README.md:47` |
| `/pgw/project-{projectId}/api/v1/*` | Same endpoints as `/project-{id}/api/v1/*`, used from inside widgets. Use the direct path for integrations. | — | `api/ifc-searches.md:11-16` |

### 0.2 Standard headers for JSON:API calls

```
Accept: application/vnd.api+json
Content-Type: application/vnd.api+json
Authorization: Bearer {idToken}
```

The docs send `Content-Type` even on GET and DELETE (`api/documents.md:53-59`). The Bearer value is the **`idToken`, not the `accessToken`** (`README.md:22-26`, `api/authentication.md:34`, `:48`).

### 0.3 IDs and timestamps

- JSON:API `id`s are strings in project-scoped resources, for example `"1002"` (`api/documents.md:101`). The `/mgw/api/v3/projects` and `project-links` examples show **integer** ids, for example `"id": 1552` (`api/projects.md:358`, `:874`). Parse both.
- The root folder has id `"0"` (`api/documents.md:1446`, `:1580`).
- Revision ids are composites: `{documentId}-{revisionNumber}`, for example `"1002-1"` (`api/documents.md:475`). File ids look like `documents-{documentId}-{revision}` (`api/documents.md:123`, `:406`).
- User ids are **email addresses** (`api/users.md:60`; for example `uploaded-by` → `{ "id": "user@example.com", "type": "users" }`, `api/documents.md:131-133`).
- Datetime attributes are ISO-8601 UTC with microseconds, for example `"2021-06-11T09:50:52.489056Z"` (`api/documents.md:111`).
- "since" filters and `meta.timestamp` use **epoch nanoseconds**: the filter takes an int64 such as `1577836800000000000`, and `meta.timestamp` is a string such as `"1624541386399325605"` (`api/documents.md:84`, `:139`, `:150`).

---

## 1. Regions and servers

### 1.1 Discovery endpoint [DOC `api/regions.md`]

| Method | URL | Auth |
|---|---|---|
| GET | `https://global.streambim.com/regions.json` | None |

This endpoint is served from `global.streambim.com`, not from a regional host (`api/regions.md:11`).

Response fields (`api/regions.md:13-21`):

| Field | Type | Description |
|---|---|---|
| `regions` | array | Region objects |
| `regions[].subdomain` | string | Subdomain used in `https://{subdomain}.streambim.com` |
| `regions[].title` | string | Display name |
| `regions[].distinctiveLanguages` | string[] | Languages in this region |
| `regions[].isDefault` | boolean | Whether this is the default region |

The docs' example lists `sweden` and `norway` (`api/regions.md:31-47`). It is illustrative only: `norway` is not in the live list.

### 1.2 Actual live response [LIVE, 2026-10-20]

```json
{
  "regions": [
    { "subdomain": "app",       "title": "Europe",    "distinctiveLanguages": null,         "isDefault": true  },
    { "subdomain": "sweden",    "title": "Sweden",    "distinctiveLanguages": [ "sv" ],     "isDefault": false },
    { "subdomain": "japan",     "title": "Japan",     "distinctiveLanguages": [ "ja", "jp" ], "isDefault": false },
    { "subdomain": "australia", "title": "Australia", "disctintiveLanguages": [ "en" ],     "isDefault": false },
    { "subdomain": "spf",       "title": "SPF",       "disctintiveLanguages": null,         "isDefault": false }
  ]
}
```

Parser notes [LIVE]:
- `distinctiveLanguages` can be `null`.
- The `australia` and `spf` entries **misspell the key** as `disctintiveLanguages`. Treat both spellings as optional.

### 1.3 Static list in the docs [DOC `README.md:11-18`, `api/authentication.md:7`, `api/users.md:34`]

`app`, `sweden`, `japan`, `australia`, `staging`, `dev`. The docs don't mention `spf`. `staging` and `dev` are not in `regions.json`.

### 1.4 How a client chooses a region

- **[DOC]** The docs only say to use `regions.json` "to discover regional subdomains" (`api/regions.md:3`).
- **[UNDOCUMENTED]** There is no lookup from email to region and no cross-region redirect. The user must pick a region, using `isDefault` as the preselection.
- **[LIVE]** Each region has its own Cognito user pool and app client (see §2.6), so an account exists in one region only. A login against the wrong region will presumably fail as "bad credentials". The error format for that is **[UNDOCUMENTED]**.
- **Recommendation:** fetch `regions.json` at startup and fall back to a hard-coded list (`app`, `sweden`, `japan`, `australia`, `spf`) if it fails. Allow a custom host for `staging` and `dev`. Store the region together with the tokens.

---

## 2. Authentication

### 2.1 Summary of supported mechanisms

| Mechanism | Endpoint | Usable by a native sync client? |
|---|---|---|
| Username/password (recommended) | `POST /auth/v1/login` | **Yes** |
| TOTP MFA second step | `POST /auth/v1/mfa/verify` | **Yes** |
| Token refresh | `POST /auth/v1/refresh` | **Yes** |
| SSO / federation | `GET /auth/v1/federation/providers`, `GET /mgw/api/v2/federation/redirecttoexternallogin` | **No documented way to get tokens back** (§2.6) |
| Project-scoped OAuth2 provider login | `GET /project-{id}/api/v1/oauth2/{provider}/login?microtoken=` | Not a login *to* StreamBIM (§2.7) |
| Legacy login | `POST /mgw/api/v2/login` | Don't use (§2.8) |
| OAuth2 authorization code + PKCE with your own client id | — | **Not documented** |
| OAuth2 device code | — | **Not documented** |
| Logout / token revoke | — | **Not documented** |

### 2.2 Password login [DOC `api/authentication.md:11-48`]

```
POST https://{region}.streambim.com/auth/v1/login
Content-Type: application/json; charset=utf-8

{"username": "user@example.com", "password": "yourPassword"}
```

Response `200` (`api/authentication.md:27-37`):

| Field | Type | Description |
|---|---|---|
| `result` | string | `SUCCESS`, or `CHALLENGE_REQUESTED` when MFA is required |
| `accessToken` | string | "OAuth2 access token". Only used for the MFA associate, enable and disable calls. |
| `refreshToken` | string | Used to refresh the idToken |
| `idToken` | string | **The Bearer token for all API calls** |
| `tokenType` | string | For example `Bearer` |
| `expiresIn` | integer | Token lifetime in seconds |
| `scope` | string | OAuth2 scope |
| `session` | string | **[INFERRED]** Present when `result = CHALLENGE_REQUESTED`. The login response table doesn't list it, but `mfa/verify` requires "the session string returned from the login response" (`api/authentication.md:230`, `README.md:32`), and the refresh response does list `session` (`:73`). |

The example below is **built from the field table**. The docs contain no literal login response.

```json
{ "result": "SUCCESS", "accessToken": "<microtoken>", "refreshToken": "<microtoken>", "idToken": "<microtoken>",
  "tokenType": "Bearer", "expiresIn": 3600, "scope": "..." }
```

```json
{ "result": "CHALLENGE_REQUESTED", "session": "AYABe..." }
```

The other fields in a CHALLENGE_REQUESTED response are **[UNDOCUMENTED]**. `3600` above is a placeholder; the real `expiresIn` is undocumented.

**[WEBAPP]** The challenge comes with **HTTP 401**, not 200. The web app's login form checks `401 == status && json.result === "CHALLENGE_REQUESTED"` and passes `json.session` on to its MFA page. Any other 401 is a wrong e-mail or password there. A client that treats every non-2xx as a refusal never reaches the MFA step. **[LIVE]** (2026-11-12, `sweden`) Confirmed: an account with MFA got its challenge as HTTP 401 with `CHALLENGE_REQUESTED` and a `session`.

Failure responses (wrong password, unknown user, locked account, user who must use SSO) have **[UNDOCUMENTED]** status codes and body shapes. **[LIVE]** (2026-11-12, `sweden`) A wrong password gave HTTP 401 with Cognito's error passed on as one string: `operation error Cognito Identity Provider: InitiateAuth, https response error StatusCode: 400, RequestID: <id>, NotAuthorizedException: Incorrect username or password.` Every Cognito error names the "Cognito Identity Provider", so that phrase says nothing about SSO.

### 2.3 MFA challenge [DOC `api/authentication.md:162-251`]

- MFA uses **TOTP authenticator apps only** (`:164`). SMS or e-mail OTP is not mentioned.
- The challenge is signalled by `result: "CHALLENGE_REQUESTED"` from `/auth/v1/login` (`:167`, `:48`) and also, possibly, from `/auth/v1/refresh` (`:72-73`).
- The docs show no challenge-type field (such as Cognito's `ChallengeName`) **[UNDOCUMENTED]**.

```
POST https://{region}.streambim.com/auth/v1/mfa/verify
Content-Type: application/json; charset=utf-8

{"username": "user@example.com", "session": "SESSION_STRING", "code": "123456"}
```

Response `200` (`:235-243`): `result`, `accessToken`, `idToken`, `tokenType`, `expiresIn`, `scope`.

> **Gap that matters for a sync client:** the `mfa/verify` response table **does not list `refreshToken`** **[UNDOCUMENTED]**.
> - If it is really missing, MFA users would have to enter TOTP again every time the idToken expires.
> - Cognito's `RespondToAuthChallenge` normally returns a refresh token [GENERAL], so it is probably present but omitted from the docs.
> - Test this first.

The response for a wrong code, and how long `session` stays valid, are **[UNDOCUMENTED]**. In Cognito a challenge session typically lasts a few minutes [GENERAL].

**[WEBAPP]** What the web app does around this call:
- It sends `{code, username, session}` to `/auth/v1/mfa/verify` and keeps the reply as its session; it reads the tokens (`idToken`), not `result`.
- A refusal whose payload has `error: "invalidCode"` is a wrong code: the page asks for another code with the **same** `session`. Any other refusal is shown as an expired session.
- After MFA it keeps `refreshToken` and refreshes as usual, except for `@rendra.io` staff accounts, whose `refreshToken` it deletes. So `mfa/verify` most likely returns a refresh token (still to confirm live).
- Refresh: `POST /auth/v1/refresh` with `{refreshToken, token: <idToken>}`, 300 s before the JWT's `exp`; a 401 ends the session.

**[LIVE]** (2026-11-12, `sweden`) Synka's MFA sign-in went through end to end: the 401 challenge, then `mfa/verify` with the `session` and a code from an authenticator app, accepted on the first try. Not seen yet: whether that reply carries a `refreshToken` (Synka does not log it), and the body of a wrong-code refusal.

### 2.4 Token refresh [DOC `api/authentication.md:52-87`]

```
POST https://{region}.streambim.com/auth/v1/refresh
Content-Type: application/json; charset=utf-8

{"token": "EXPIRED_ID_TOKEN", "refreshToken": "YOUR_REFRESH_TOKEN"}
```

Note that the field is **`token`**, and its value is the *expired* idToken, not the accessToken.

Response `200` (`:68-78`):

| Field | Type | Description |
|---|---|---|
| `result` | string | `SUCCESS` or `CHALLENGE_REQUESTED` |
| `session` | string | Session for MFA verification, present when `CHALLENGE_REQUESTED` |
| `accessToken` | string | New access token |
| `idToken` | string | **New Bearer token** |
| `tokenType` | string | Token type |
| `expiresIn` | integer | Lifetime in seconds |
| `scope` | string | OAuth2 scope |

Things the docs leave open:
- **[INFERRED]** No new `refreshToken` is returned, so keep the original one. Cognito doesn't rotate refresh tokens by default [GENERAL].
- **[INFERRED]** On `CHALLENGE_REQUESTED`, call `/auth/v1/mfa/verify` with the returned `session` and a fresh TOTP code.
- **[UNDOCUMENTED]** Refresh-token lifetime. Cognito's default is 30 days, but it's configurable [GENERAL].
- **[UNDOCUMENTED]** Whether a *not-yet-expired* idToken is accepted, which would allow proactive refresh. The docs only say "expired".
- **[UNDOCUMENTED]** The error response when the refresh token itself has expired or been revoked. Treat any non-200 response, or a 200 without `idToken`, as "full re-login needed".

### 2.5 Using the tokens

| Use | How | Source |
|---|---|---|
| All REST calls | `Authorization: Bearer {idToken}` | `README.md:22-26`; every section |
| Token format | JWT ("Bearer token required (JWT)") | `api/bcf.md:6` |
| JWT claims (email, exp, …) | **[UNDOCUMENTED]**. Cognito id tokens normally carry `email`, `exp` and `cognito:username` [GENERAL]. | — |
| Tileserver images and tiles | Query `?id_token={idToken}`, because they're loaded by `<img>` tags | `api/tileserver.md:7`, `:34`, `:113` |
| Download, viewer and OAuth URLs | Query `?microtoken={microtoken}` (§6.3) | `api/attachments.md:930-968`, `api/tileserver.md:101`, `api/authentication.md:295` |
| Cookies | Not used or documented | — |
| `accessToken` | Only in the body of `mfa/associate`, `mfa/enable` and `mfa/disable` ("NOT the ID token") | `api/authentication.md:181`, `:202`, `:216` |

Recommended token-management loop:
1. Store `{region, username, idToken, refreshToken, expiresAt = now + expiresIn}`. Keep the refresh token in the OS keychain.
2. Before each call, or when a call returns `401`, call `/auth/v1/refresh`. A documented `401` means "Not authorized. Log in via POST /auth/v1/login" (`api/users.md:114`).
3. If refresh returns `CHALLENGE_REQUESTED`, prompt for a TOTP code and call `mfa/verify` with the returned session.
4. If refresh fails, prompt for the password again.

### 2.6 SSO / federation [DOC `api/authentication.md:141-158`, `:304-322`]

**List providers:**

```
GET https://{region}.streambim.com/auth/v1/federation/providers
```

The response is an array of `{ "name": string, "loginURL": string }`. The default provider is `COGNITO` (`:145-151`).

**Browser redirect to an external IdP:**

```
GET https://{region}.streambim.com/mgw/api/v2/federation/redirecttoexternallogin?provider={name}
```

This returns `302` to the external identity provider. It is a browser endpoint, not a JSON API (`:306-322`).

**What the live servers return [LIVE]** (one `COGNITO` provider per region):

| Region | Cognito hosted-UI host | `client_id` | `redirect_uri` |
|---|---|---|---|
| app | `eu-central-1hcpwawzhy.auth.eu-central-1.amazoncognito.com` | (StreamBIM's own, omitted) | `https://app.streambim.com/auth/v1/federation` |
| sweden | `eu-central-1yuaugz0f2.auth.eu-central-1.amazoncognito.com` | (StreamBIM's own, omitted) | `https://sweden.streambim.com/auth/v1/federation` |
| japan | `ap-northeast-1ta9rkskmj.auth.ap-northeast-1.amazoncognito.com` | (StreamBIM's own, omitted) | `https://japan.streambim.com/auth/v1/federation` |
| australia | `ap-southeast-2ojws6aecl.auth.ap-southeast-2.amazoncognito.com` | (StreamBIM's own, omitted) | `https://australia.streambim.com/auth/v1/federation` |
| spf | `eu-central-1borqlx5q3.auth.eu-central-1.amazoncognito.com` | (StreamBIM's own, omitted) | *(empty)* |

All of them use `/oauth2/authorize?identity_provider=COGNITO&response_type=CODE&scope=email openid phone profile`.

What this means for a sync client:
- These are **StreamBIM's own** Cognito app clients. Their redirect URI is StreamBIM's server-side callback `/auth/v1/federation`, and that callback's behavior is **[UNDOCUMENTED]**.
- A third-party desktop app can't register its own redirect URI (loopback or custom scheme) and so can't receive the code.
- PKCE isn't present. How StreamBIM tokens reach the client after the callback is **[UNDOCUMENTED]**.
- **Conclusion:** SSO-only users can't sign in to a native client in any documented way. Neither "bring your own client id" nor OIDC client registration is documented.

**Related:** project membership can restrict a user to specific SSO providers through `sso-providers`. An empty list means all methods are allowed (`api/users.md:153`, `:270`; `api/projects.md:48`). **[INFERRED]** Users restricted this way may not be able to use password login.

### 2.7 Project-scoped OAuth2 provider login [DOC `api/authentication.md:278-302`]

1. `GET /project-{projectId}/api/v1/microtoken`
2. Open in a browser: `GET /project-{projectId}/api/v1/oauth2/{provider}/login?microtoken={microtoken}`

This starts an OAuth2 flow "through a third-party provider within a project context". The provider identifiers are **[UNDOCUMENTED]**.

**[INFERRED]** StreamBIM acts as the OAuth client of an external system here, for example to connect the Interaxo integration. Compare the `INTERAXO_REFRESH_TOKEN` project config in `api/sync.md:141`. This is **not** a way for a third-party app to authenticate to StreamBIM.

### 2.8 Legacy login [DOC `api/authentication.md:326-357`]

`POST /mgw/api/v2/login` with `{"username","password"}` returns `{accessToken, refreshToken}`. It returns no `idToken` and documents no MFA handling. **Don't use it.**

### 2.9 Logout

**[UNDOCUMENTED]** There is no logout, sign-out or revoke endpoint. Delete the tokens locally.

### 2.10 Other auth endpoints (not needed for sync)

- `POST /auth/v1/mfa/associate` with body `{accessToken}` returns `{accessToken, secretCode, qrCodeValue}` (`:170-189`).
- `POST /auth/v1/mfa/enable` with body `{accessToken, code}` (`:191-203`).
- `POST /auth/v1/mfa/disable` with body `{accessToken}` (`:205-216`).
- `GET /auth/v1/mfa/status` with Bearer auth. The response shape is undocumented, and it's unavailable for SSO users (`:255-275`).
- `POST /auth/v1/change-password/request` and `/confirm` (`:91-139`).

---

## 3. Current user / profile

There is **no `/me` endpoint**. These are the two options:

### 3.1 `GET /mgw/api/v2/user-profiles?filter[profile]=true` [DOC `api/users.md:1341-1367`]

```
GET https://{region}.streambim.com/mgw/api/v2/user-profiles?filter%5Bprofile%5D=true
Authorization: Bearer {idToken}
Accept: application/vnd.api+json
```

"Returns the profile for the authenticated user." The response schema is **[UNDOCUMENTED]**. It is probably a JSON:API user resource, but that isn't verified.

### 3.2 `GET /mgw/api/v3/users/{email}?filter[project]={projectId}` [DOC `api/users.md:787-833`]

- If `userID` is the current user and `filter[project]` is set, the response includes allowed providers, organization ID and customer-user-profile info.
- Returned attributes: `first-name`, `last-name`, `name`, `phone`, `title`, `allowed-providers`, `expiration-date`, `language`, `last-login`, `guest`, `organization`, `role`, `time-zone`, plus a `user-organization` relationship (`:818`).
- **Don't** pass `filter[setLastLogin]=true`. It has a side effect: it updates the last-login date (`:812-814`).
- Errors: `400` for a bad filter; `404` for "Insufficient privileges or user not found" (`:822-825`).
- You need the email. Use the login `username`; usernames are emails (`api/authentication.md:24`).

**User schema** (`api/users.md:54-71`): `id` (email), `name`, `first-name`, `last-name`, `organization`, `title`, `phone`, `language`, `guest`, `time-zone`, `expiration-date`, `role` (`customer` | `rendra_staff`).

---

## 4. Projects the user can access

### 4.1 Endpoints

| Endpoint | Returns | Notes / source |
|---|---|---|
| `GET /mgw/api/v3/project-links` | Lightweight snippets | Best for a project picker. Whether it is scoped to the user isn't stated, but it's presumably the user's projects. `api/projects.md:840-895` |
| `GET /mgw/api/v3/projects` | Full Project objects | "Retrieves all projects" (scope unclear). Filters: `filter[active]`, `filter[hibernated]`, `filter[isLibrary]`, `filter[externalId]`. `api/projects.md:300-402` |
| `GET /mgw/api/v3/projects/{projectID}` | One Project | `api/projects.md:486-515` |
| `GET /mgw/api/v3/customer-projects` | CustomerProject list ("similar to Project") | "Returns all projects the user has access to. Only core properties". Enterprise users. `api/projects.md:598-631` |
| `GET /project-{id}/api/v1/v2/projects/{id}` | Project, project-scoped | `api/projects.md:562-594` |
| `GET /bcf/2.1/projects` | `[{project_id (uuid), name, authorization.project_actions[]}]` | Actions include `"createDocument"`, which is useful for write permission. `api/bcf.md:25-64` |

**Pagination:** none is documented for the `/mgw` project lists. Expect the whole list in one response.

### 4.2 `project-links` example [DOC `api/projects.md:870-886`]

```json
{
  "data": [
    {
      "id": 1552,
      "type": "project-links",
      "attributes": {
        "name": "PROJECT_NAME",
        "description": "DESCRIPTION",
        "external-id": "EXT-001",
        "is-hibernated": false,
        "is-library": false,
        "project-uri": "https://dev.streambim.com/webapp/default/#/?projectId=1552"
      }
    }
  ]
}
```

### 4.3 Project schema (from `/mgw/api/v3/projects`) [DOC `api/projects.md:324-394`]

| Field | Type | Notes |
|---|---|---|
| `id` | integer | Project ID. This is the `{projectId}` in all project-scoped paths. |
| `attributes.name` | string | |
| `attributes.description` | string | |
| `attributes.bcf-id` | uuid | **[INFERRED]** Same value as BCF `project_id` |
| `attributes.external-id` | string | |
| `attributes.project-uri` | string | Web URL. The example at `:371` has no scheme (`dev.streambim.com/webapp/...`); the example at `:882` has one. |
| `attributes.status` | string | `Running` \| `Starting` \| `Crashed` \| `Stopped` |
| `attributes.is-hibernated` | bool | Read-only, owner-only access (`:338`) |
| `attributes.is-inactive` | bool | Hidden; deleted after 30 days (`:339`) |
| `attributes.is-library` | bool | Library rather than project |
| `attributes.created-at`, `deleted-at` | date-time | |
| `attributes.coords`, `salesforce-*`, `sync_template` | — | Not relevant |
| `relationships.customer` | `{id, type:"customers"}` | **Company/org grouping.** The example has `"id": 0` (`:379`). |
| `relationships.owner` | `{id: email, type:"users"}` | |
| `relationships.features` | `[{id, type:"features", attributes:{enabled}}]` | Feature flags |

**Org/company grouping:**
- Projects carry only a `customer` id.
- Customer names come from `GET /mgw/api/v2/customers`. It's for enterprise accounts and its schema is undocumented (`api/users.md:711-735`).
- `user-organizations` describes *user* companies, not project grouping (`api/users.md:426-491`).
- `project-links` has **no** customer field.

### 4.4 Project lifecycle states that a sync client must handle

- **Stopped project:** a project inactive for more than 2 weeks must be started with `GET /mgw/api/v1/projects/{projectID}/_starttrigger`.
  - Success returns `200` with the body "Project started"; `401` means no access (`api/projects.md:519-554`).
  - **[INFERRED]** Call it when `status` is `Stopped`, or when project-scoped calls fail, then poll until `status` is `Running`.
- **`is-hibernated: true`:** treat the project as read-only (`api/projects.md:338`).
- **Stats:** `GET /project-{id}/api/v1/v2/project-stats/{id}` returns `num-documents`, `num-folders` and enabled `features`, for example `INTERAXO` (`api/projects.md:982-1048`). Useful as a sanity check and to spot projects synced from Interaxo.

---

## 5. Folders and documents

### 5.1 Model [DOC `api/documents.md:3`, `:45`, `:1446`]

- Documents live in folders. A document can have one or more revisions.
- **Permissions are set at folder level.** Labels can be put on both folders and documents.
- The root folder has id `0`.

### 5.2 List folders

```
GET /project-{projectId}/api/v1/v2/folders
```

Source: `api/documents.md:1448-1517`. Filters:

| Param | Type | Default | Description |
|---|---|---|---|
| `filter[isDeleted]` | bool | `null` | Filter by deleted status |
| `filter[parent]` | int | `null` | Parent folder ID. `0` returns root-level folders only. |
| `filter[since]` | int64 | `null` | Epoch **nanoseconds** |

- The response is "filtered by the current user's access rights" (`:1452`).
- No pagination is documented for folders.
- **[UNDOCUMENTED]** Whether `filter[isDeleted]` left at `null` returns deleted *and* live folders. Pass it explicitly.

```json
{
  "data": [
    {
      "id": "7",
      "type": "folders",
      "attributes": {
        "name": "STREAMBIM",
        "description": "",
        "created-date": "2021-06-10T13:44:56.50162Z",
        "last-modified": "2021-06-15T00:30:00.627941Z",
        "access-level": 20,
        "is-deleted": false,
        "external-id": "7",
        "deleted-date": null
      },
      "relationships": {
        "modified-by": { "data": { "id": "user@example.com", "type": "users" } },
        "folder-accesses": { "data": [] },
        "parent": { "data": { "id": "0", "type": "folders" } },
        "labels": { "data": [] }
      }
    }
  ]
}
```

**Folder schema** (`:1681-1701`):

| Attribute | Type | Nullable | Notes |
|---|---|---|---|
| `name` | string | no | |
| `description` | string | no | |
| `created-date` | date-time | yes | |
| `last-modified` | date-time | yes | |
| `access-level` | int | no | Example `20`. **[INFERRED]** On the privilege scale 0/5/10/15/20/25/30 (`README.md:121-133`). Its exact meaning (the user's effective access to the folder?) is **[UNDOCUMENTED]**. |
| `is-deleted` | bool | no | Soft-delete flag |
| `external-id` | string | yes | For integration partners |
| `deleted-date` | date-time | yes | |

Relationships:
- `modified-by`: user email.
- `folder-accesses`: composite id `{AccessID}-{FolderID}`.
- `parent`: `null` for root (`:1700`). Top-level folders point at `"0"`.
- `labels`.

Folders have no `path` attribute. Build the tree from `parent` ids.

**Single folder:** `GET /project-{projectId}/api/v1/v2/folders/{folderId}` (`:1521-1554`). The web app fetches `GET folders/0` (`api/ifc-files.md:209`).

### 5.3 List documents

```
GET /project-{projectId}/api/v1/v2/documents
```

Source: `api/documents.md:47-161`. "Filtered by the current user's access rights" (`:51`).

| Param | Type | Default | Description |
|---|---|---|---|
| `filter[text]` | string | `""` | Free-text search |
| `filter[fileExtensions]` | string[] | — | The example is comma-separated: `pdf,ifc` (`:160`) |
| `filter[labelIds]` | int[] | — | Encoding unspecified. `api/ifc-files.md:198` shows `filter[labels]=["1025"]` (a different name, JSON array). |
| `filter[noLabels]` | bool | `false` | |
| `filter[buildingId]`, `filter[floorId]`, `filter[publishedIfc]`, `filter[starred]` | — | — | Not relevant for sync |
| `filter[documentIds]` | int[] | — | Specific IDs |
| `filter[folderId]` | int | `null` | Documents directly in this folder **[INFERRED]** |
| `filter[fromFolderId]` | int | `null` | "Filter from a specific folder ID". **[UNDOCUMENTED]** Possibly a whole subtree. |
| `filter[isDeleted]` | bool | `null` | Deleted or non-deleted |
| `filter[since]` | int64 | `null` | Epoch nanoseconds, for example `1577836800000000000` |
| `filter[externalId]` | string | `""` | Integration-partner ID |
| `page[skip]` | int | `0` | Offset |
| `page[limit]` | int | `null` | `null` means **all** |

**Constraint** (`:69`): the "search" filters (`labels`, `starred`, `freetext`, `fileExtensions`, `noLabels`) **cannot be combined with `isDeleted=true` and `since`**. Doing so returns `400` "bad request". The text is ambiguous about whether this means "isDeleted=true *or* since" or both together. Avoid combining search filters with either.

Response example (`:97-142`):

```json
{
  "data": [
    {
      "id": "1002",
      "type": "documents",
      "attributes": {
        "filename": "11.jpg",
        "description": "",
        "num-revisions": 1,
        "scale": null,
        "filesize": 406602,
        "path": "STREAMBIM",
        "revision": 1,
        "last-modified": "2021-06-11T09:50:52.489056Z",
        "uploaded-date": "2021-06-11T09:50:52.489056Z",
        "is-deleted": false,
        "external-id": null,
        "deleted-date": null
      },
      "relationships": {
        "document-alignments": { "data": [] },
        "document-revision": { "data": { "id": "1002-1", "type": "document-revisions" } },
        "file": { "data": { "id": "documents-1002-1", "type": "files" } },
        "labels": { "data": [] },
        "parent": { "data": { "id": "7", "type": "folders" } },
        "starred": { "data": null },
        "upload-ticket": { "data": null },
        "uploaded-by": { "data": { "id": "user@example.com", "type": "users" } }
      }
    }
  ],
  "meta": {
    "labelIds": { "1001": true, "1002": false },
    "timestamp": "1624541386399325605",
    "total": 5
  }
}
```

**Meta** (`:145-151`):
- `total`: total matching documents.
- `timestamp`: server time in Unix **nanoseconds**, as a string.
- `labelIds`: map of label id to boolean.

**Document schema** (`:382-411`):

| Attribute | Type | Nullable | Sync relevance |
|---|---|---|---|
| `filename` | string | no | File name, which is also the local name |
| `description` | string | yes | |
| `num-revisions` | int | no | |
| `scale` | int | yes | |
| `filesize` | int64 | no | Bytes of the current revision |
| `path` | string | no | "Folder path", for example `"STREAMBIM"`. The separator is **[UNDOCUMENTED]**; the export example has `"MODELS/ARK"` (`api/exports.md:300`). Does it include the root? Unknown. Prefer building paths from the folder tree. |
| `revision` | int | yes | **Current revision number.** This is the version id to compare. |
| `last-modified` | date-time | yes | |
| `uploaded-date` | date-time | yes | |
| `is-deleted` | bool | no | Soft-delete |
| `external-id` | string | yes | Integration-partner id. Could tag documents the client created; see §11. |
| `deleted-date` | date-time | yes | |

Relationships:
- `document-revision`: current revision, `"{docId}-{rev}"`.
- `file`: `"documents-{docId}-{rev}"`.
- `parent`: folder.
- `uploaded-by`: user email.
- `labels`, `starred`, `upload-ticket`, `document-alignments`, `ifc-files`.

**Missing fields that a sync client would want:**
- **No checksum, hash or ETag** on documents or revisions **[UNDOCUMENTED / absent]**.
- **No `modified-by`** on documents (only `uploaded-by`). Folders do have `modified-by`.
- No MIME type field.

**Single document:** `GET /project-{projectId}/api/v1/v2/documents/{documentId}` returns the same schema (`:165-209`).

### 5.4 Revisions

```
GET /project-{projectId}/api/v1/v2/document-revisions?filter[document]={documentId}
```

Source: `api/documents.md:477-537`. No pagination is documented.

```json
{
  "data": [
    {
      "id": "1002-1",
      "type": "document-revisions",
      "attributes": {
        "description": "",
        "filesize": 406602,
        "is-active-revision": true,
        "revision": 1,
        "uploaded-date": "2021-06-11T09:50:52.489056Z"
      },
      "relationships": {
        "document": { "data": { "id": "1002", "type": "documents" } },
        "file": { "data": { "id": "documents-1002-1", "type": "files" } },
        "upload-ticket": { "data": null },
        "uploaded-by": { "data": { "id": "user@example.com", "type": "users" } }
      }
    }
  ]
}
```

- Single revision: `GET /project-{projectId}/api/v1/v2/document-revisions/{docId}-{rev}` (`:541-574`).
- `is-active-revision` exists, which suggests the active revision might not always be the newest. The rules for this are **[UNDOCUMENTED]**.

### 5.5 Flat alternative: JSON export

`GET /project-{projectId}/api/v1/documents/export/json/?query={base64(JSON)}` returns flat (non-JSON:API) document rows. The two docs files **contradict each other**:

| | `api/documents.md:1760-1838` | `api/exports.md:245-331` |
|---|---|---|
| Query shape | `{"filter":{"freetext","isDeleted","fileExtensions","labelIds","folderId"}}` | `{"skip","limit","selectors":{"freetext","fileExtensions","labelIds","folderId","isDeleted","building","starred"},"filter":{}}` |
| `query` required? | Yes | No: omitting it returns all documents |
| Response row | `id` (string), `filename`, `uploadedDate`, `filesize`, `revision` | `id` (int), `filename`, `description`, `revision`, `numRevisions`, `filesize`, `path`, `uploadedDate`, `lastModified`, `isDeleted`, `labels[]` (names), `folderId` |

Use the JSON:API `/v2/documents` endpoint as the primary listing. Treat the export endpoint as optional.

### 5.6 Change detection / delta feed

- **[DOC]** There's no dedicated changes feed. Documents (`api/documents.md:84`) and folders (`:1474`) both accept `filter[since]=<epoch ns>`. The documents listing returns `meta.timestamp` in epoch ns (`:139`, `:150`).
- **[DOC]** `isDeleted` may be combined with `since`. Only the *search* filters may not (`:69`).
- **[INFERRED] Cursor pattern** (verify with a live test):
  1. Full scan: `GET folders?filter[isDeleted]=false` plus paged `GET documents?filter[isDeleted]=false`. Save `cursor = meta.timestamp` from the **first** page, so changes made during the scan aren't missed.
  2. Incremental, live items: `GET folders?filter[since]={cursor}` and `GET documents?filter[since]={cursor}&filter[isDeleted]=false`.
  3. Incremental, deletions: `GET documents?filter[since]={cursor}&filter[isDeleted]=true`, and the same for folders.
  4. Advance `cursor` to the new `meta.timestamp`. Folders have no documented `meta.timestamp`, so use the one from the documents call.
- **[UNDOCUMENTED]** What `since` compares against: `last-modified`, or `deleted-date` too? Do moves, renames and label changes bump `last-modified`? Does a folder move bump its child documents? Does deleting a folder cascade, and does it bump the children? Run a full re-scan periodically as a safety net.
- **[DOC]** SSE (`GET /project-{id}/api/v1/sse/stat`, `/sse/notifications`) only documents the event types `report-export`, `users-import` and `report-import`. There are **no document-change events** (`api/realtime.md:55-61`). Polling is required.
- **[DOC]** The web app itself polls `GET /v2/last-modifieds/{buildingId}` and `GET /v2/projects/{id}` about every 30 s (`api/sync.md:282`). `last-modifieds` is per *building* (3D model), not per document library (`api/projects.md:1052-1092`).

---

## 6. Download

### 6.1 Get a download link

```
GET /project-{projectId}/api/v1/documents/{documentId}/downloadlink
Authorization: Bearer {idToken}
```

This returns a link for the document's **current revision**. Note the path has no `v2`. The docs give **two contradictory response formats**:

| Variant | Source | Response | Full URL |
|---|---|---|---|
| A | `api/documents.md:419-467` (sent with `Accept: application/json`) | `{"data": "/documents/1002/1/download?token=..."}` | `https://{region}.streambim.com/project-{projectId}/api/v1{data}` |
| B | `api/documents.md:1707-1756` | Plain text: `documents/1002/download/filename.pdf` | `https://{region}.streambim.com/project-{projectId}/api/v1/{downloadLink}`. The curl example also sends `Authorization: Bearer` on the second GET. |

**Robust client approach:**
1. Send `Accept: application/json`.
2. If the body parses as JSON with a string `data` field, use it. Otherwise use the trimmed body text and strip surrounding quotes if present.
3. Join to `/project-{id}/api/v1` with exactly one `/`.
4. GET the result. Include the Bearer header when the link has no `token=` or `microtoken=` parameter.

More details:
- "The download link is temporary and expires after a short period. Request a new link if it has expired" (`:467`). The exact lifetime is **[UNDOCUMENTED]**.
- The second GET's response is "the raw file content" (`:465`), "with the appropriate `Content-Type` header" (`:1742`).
- **[UNDOCUMENTED]** Whether the document download answers `302` to S3. Attachment downloads do redirect to a presigned S3 URL (`api/attachments.md:209`, `:233`).
  - **[GENERAL]** Follow redirects, but **don't forward `Authorization` to the S3 host**. S3 rejects a presigned URL that also carries an Authorization header. Most HTTP clients, `reqwest` included, drop `Authorization` on cross-origin redirects.
- **[UNDOCUMENTED]** HTTP `Range` and resume support, `Content-Length`, and `ETag`. In variant A the `/1/` segment in `/documents/1002/1/download` looks like the revision number.
- **[UNDOCUMENTED]** How to download a **specific older revision**. Only the current revision's link is documented.

### 6.2 `/downloads/{id}` endpoints

`GET /project-{projectId}/api/v1/downloads/{downloadId}[/{fileName}]?microtoken=` serves generated reports and exports, not documents (`api/attachments.md:972-1019`).

### 6.3 Microtoken (used for browser and viewer URLs)

```
GET /project-{projectId}/api/v1/microtoken
Authorization: Bearer {idToken}
```

The response is `{"microtoken": "<microtoken>"}`. Append `?microtoken={token}` to download, viewer and tileserver URLs (`api/attachments.md:934-968`). It is "short-lived"; the exact lifetime is **[UNDOCUMENTED]**.

### 6.4 Thumbnails (optional)

`GET /project-{projectId}/api/v1/v2/tileserver/images/{fileId}?page=0&size=400&id_token={idToken}` returns PNG/JPEG (`api/tileserver.md:13-43`). The example uses a document id as `fileId` (`1002`). Whether it should be `documents-1002-1` is **[UNDOCUMENTED]**.

---

## 7. Upload

### 7.1 Overview [DOC `api/attachments.md:19-32`]

1. `POST /project-{projectId}/api/v1/v2/upload-tickets` returns a presigned S3 URL and a ticket `id`.
2. `PUT {s3presignedurl}` with the raw bytes. This goes directly to S3 with no StreamBIM auth.
3. Register the upload:
   - **New document:** `POST /project-{projectId}/api/v1/v2/documents`
   - **New revision:** `POST /project-{projectId}/api/v1/v2/document-revisions`

Step 3 must happen "strictly after the S3 upload has finished" (`:32`). "Provide the correct filename and size in the upload ticket request; mismatches can cause upload failures" (`:32`).

The docs only spell out the attachment version of step 3. For documents, the `upload-ticket` relationship in Create Document (`api/documents.md:246`, `:269-274`) uses the same ticket.

### 7.2 Step 1: create an upload ticket [DOC `api/attachments.md:247-325`]

```
POST https://{region}.streambim.com/project-{projectId}/api/v1/v2/upload-tickets
Accept: application/vnd.api+json
Content-Type: application/vnd.api+json
Authorization: Bearer {idToken}

{
  "data": {
    "id": "0",
    "type": "attachments",
    "attributes": {
      "filename": "070e4e49da6921a4686be369662bec8f.jpg",
      "file-size": 65304
    }
  }
}
```

- The request `type` is **`"attachments"`** in the docs. Whether `"upload-tickets"` is also accepted is **[UNDOCUMENTED]**.
- The size attribute is **`file-size`** (hyphenated). The document resource uses `filesize`.

Response `200`:

```json
{
  "data": {
    "id": "4e59d246-beca-4ee5-b47c-40c7f5ca2a19",
    "type": "upload-ticket",
    "attributes": {
      "awskey": "<YOUR_AWS_ACCESS_KEY_ID>",
      "awsregion": "eu-central-1",
      "awsurl": "https://s3-eu-central-1.amazonaws.com",
      "file-size": 65304,
      "filename": "070e4e49da6921a4686be369662bec8f.jpg",
      "last-modified": 0,
      "s3bucket": "rendra-eu-central-1",
      "s3key": "rendra-projects/development/project-1201/uploads/4e59d246-beca-4ee5-b47c-40c7f5ca2a19",
      "s3presignedurl": "https://rendra-eu-central-1.s3.eu-central-1.amazonaws.com/...",
      "signer-url": "https://dev.streambim.com/project-1201/api/v1/aws/_uploadsigner",
      "token": ""
    }
  }
}
```

Use `s3presignedurl` for the PUT and `data.id` (a UUID) as the ticket id (`:305`).

### 7.3 Step 2: PUT to S3 [DOC `api/attachments.md:329-357`]

```
PUT {s3presignedurl}
Content-Type: <file MIME type, e.g. application/pdf>

<raw bytes>
```

- No StreamBIM auth headers (`:333`).
- `200 OK` means success. `4xx`/`5xx` means failure, for example an invalid URL or a size mismatch (`:345-348`).
- The example presigned URL carries `X-Amz-Expires=300&X-Amz-SignedHeaders=host` (`:356`).
  - **[INFERRED]** The URL is valid for about **5 minutes**, so request the ticket right before uploading.
  - Only `host` is signed, so the `Content-Type` value isn't enforced by the signature.
  - **[GENERAL]** S3 checks expiry when the request arrives, so a long upload that *starts* in time should finish. Verify this.
- **[GENERAL]** A single S3 `PUT` is limited to **5 GB**.

### 7.4 Large files / multipart: **[UNDOCUMENTED]**

- The ticket also returns `awskey`, `awsregion`, `awsurl`, `s3bucket`, `s3key` and `signer-url = https://{region}.streambim.com/project-{id}/api/v1/aws/_uploadsigner`.
- **[INFERRED]** These fields match the pattern of browser S3 multipart libraries such as EvaporateJS, where a server endpoint signs each part.
- The `_uploadsigner` request and response protocol isn't documented anywhere. Anything over 5 GB, or chunked and resumable uploads, needs a live test or input from Rendra.
- StreamBIM's own size limits are **[UNDOCUMENTED]**.

### 7.5 Alternative to step 2: form upload to StreamBIM [DOC `api/attachments.md:1049-1085`]

```
POST /project-{projectId}/api/v1/documents/_upload
Authorization: Bearer {idToken}
Content-Type: multipart/form-data

form fields:  file=<binary>   id=<upload ticket id>
```

The documented flow is "1. Create an upload ticket record. 2. POST the file as multipart form data to `/documents/_upload` with the ticket ID."

**[INFERRED]** This replaces only the S3 PUT, and you still register the document or revision afterwards. Whether this endpoint creates the document record by itself is **[UNDOCUMENTED]**.

### 7.6 Step 3a: create a new document [DOC `api/documents.md:213-279`]

```
POST https://{region}.streambim.com/project-{projectId}/api/v1/v2/documents
Accept: application/vnd.api+json
Content-Type: application/vnd.api+json
Authorization: Bearer {idToken}

{
  "data": {
    "id": "0",
    "type": "documents",
    "attributes": {
      "description": "",
      "filename": "floorplan.pdf",
      "filesize": 406602
    },
    "relationships": {
      "labels": { "data": [] },
      "parent": { "data": { "id": "7", "type": "folders" } },
      "upload-ticket": {
        "data": { "type": "upload-tickets", "id": "8b6ed287-8c2e-4f40-b99c-74d1dd1c5a28" }
      }
    }
  }
}
```

| Field | Required | Notes |
|---|---|---|
| `data.id` | yes | `"0"` for new documents |
| `data.type` | yes | `"documents"` |
| `attributes.filename` | yes | **[INFERRED]** Must match the ticket filename |
| `attributes.filesize` | yes | Bytes. **[INFERRED]** Must match the ticket and the S3 object. |
| `attributes.description` | no | |
| `attributes.external-id` | no | |
| `attributes.scale` | no | |
| `relationships.parent.data` | yes | `{ "id": "<folderId>", "type": "folders" }` |
| `relationships.upload-ticket.data` | yes | `{ "id": "<ticketId>", "type": "upload-tickets" }` |
| `relationships.labels.data` | no | `[{ "id", "type": "document-labels" }]` |

- **Response:** **[UNDOCUMENTED]**. Presumably the created Document resource with its server-assigned `id`.
- **[UNDOCUMENTED]** Whether two documents with the same `filename` can exist in one folder. If they can, the client must map by id and handle local name collisions.
- **[UNDOCUMENTED]** Rules on allowed characters and lengths.
- **[UNDOCUMENTED]** How quickly the new document shows up in listings. The server may process it asynchronously.

### 7.7 Step 3b: create a new revision of an existing document [DOC `api/documents.md:578-623`]

```
POST https://{region}.streambim.com/project-{projectId}/api/v1/v2/document-revisions
Accept: application/vnd.api+json
Content-Type: application/vnd.api+json
Authorization: Bearer {idToken}

{
  "data": {
    "id": "1002-2",
    "type": "document-revisions",
    "attributes": {
      "description": "",
      "filesize": 406602
    },
    "relationships": {
      "upload-ticket": { "data": { "type": "upload-tickets", "id": "<ticketId>" } }
    }
  }
}
```

| Field | Required | Notes |
|---|---|---|
| `data.id` | yes | Composite `{DocumentID}-{NewRevisionNumber}`. **[INFERRED]** The new number is the document's current `revision` + 1. |
| `data.type` | yes | `"document-revisions"` |
| `attributes.filesize` | yes | |
| `attributes.description` | no | |
| `relationships.upload-ticket.data` | "No" per the table | **Contradiction:** the docs' example sends `"upload-ticket": { "data": null }` (`:618`), which can't link any bytes. **[INFERRED]** Send the ticket from step 1. |

- There's no `document` relationship in the request. The parent document is identified only by the composite id prefix.
- There's no `filename` on revisions. **[INFERRED]** A new revision keeps the document's filename; rename with PATCH on the document (§8).
- **[UNDOCUMENTED]** Response body. Also the concurrency behavior when the chosen revision number already exists. That could serve as a natural optimistic-concurrency check (expect 409 or 400), but it's unverified.

---

## 8. Folder and document mutations

### 8.1 Folders [DOC `api/documents.md:1558-1677`]

| Operation | Request |
|---|---|
| **Create** | `POST /project-{projectId}/api/v1/v2/folders`. `data.id` "can be any string (server assigns actual ID)" (`:1576`). The response is the created Folder with the server id (`:1584`). |
| **Rename** | `PATCH /project-{projectId}/api/v1/v2/folders/{folderId}` with `attributes.name` |
| **Move** | Same PATCH with `relationships.parent.data = { "id": "<newParentId>", "type": "folders" }` ("Move folder to a different parent", `:1647`) |
| **Delete (soft)** | Same PATCH with `attributes.is-deleted: true` (`:1645`) |
| **Restore** | **[INFERRED]** Same PATCH with `attributes.is-deleted: false`. The field accepts a boolean; restore semantics aren't documented. |
| **Hard delete** | **No `DELETE /folders/{id}` is documented** |

Create example (`:1593-1611`):

```json
{
  "data": {
    "id": "1",
    "type": "folders",
    "attributes": { "name": "NewFolderName" },
    "relationships": {
      "labels": { "data": [ { "id": "1005", "type": "document-labels" } ] },
      "parent": { "data": { "id": "0", "type": "folders" } }
    }
  }
}
```

PATCH example (`:1656-1674`):

```json
{
  "data": {
    "id": "8",
    "type": "folders",
    "attributes": { "name": "RenamedFolder", "is-deleted": false },
    "relationships": {
      "labels": { "data": [ { "id": "1005", "type": "document-labels" } ] },
      "parent": { "data": { "id": "0", "type": "folders" } }
    }
  }
}
```

**[UNDOCUMENTED]**:
- Whether soft-deleting a folder cascades to its subfolders and documents.
- Whether PATCH is a true JSON:API partial update. The examples always send `labels` and `parent`; omitting them might clear them. Echo the current `labels` and `parent` in every PATCH.
- Which privilege is needed to create, move or delete folders (folder-level permissions, `:1446`).
- Whether new folders get a description.

### 8.2 Documents [DOC `api/documents.md:283-378`]

| Operation | Request |
|---|---|
| **Rename** | `PATCH /project-{projectId}/api/v1/v2/documents/{documentId}` with `attributes.filename` (`:311`) |
| **Move** | Same PATCH with `relationships.parent.data` = new folder (`:316`) |
| **Delete (soft)** | Same PATCH with `attributes.is-deleted: true` (`:314`) |
| **Restore** | **[INFERRED]** Same PATCH with `attributes.is-deleted: false` |
| **Delete** | `DELETE /project-{projectId}/api/v1/v2/documents/{documentId}`. The docs only say "Delete a document" (`:349-378`); whether it's a hard or soft delete is **[UNDOCUMENTED]**. **[INFERRED]** Prefer PATCH `is-deleted: true` for sync, because it's recoverable. |
| **Delete a single revision** | **Not documented** |

PATCH example (`:325-343`). It sends the full attribute set:

```json
{
  "data": {
    "id": "1002",
    "type": "documents",
    "attributes": {
      "description": "Updated description",
      "filename": "11.jpg",
      "scale": 0,
      "external-id": "ext-001",
      "is-deleted": false
    },
    "relationships": {
      "labels": { "data": [] },
      "parent": { "data": { "id": "7", "type": "folders" } }
    }
  }
}
```

The PATCH prose says "Optional fields: `scale`, `external-id`" (`:287`), but the field table also lists `description`, `filename`, `is-deleted`, `labels` and `parent` (`:306-316`). **[INFERRED]** Send the full current object with the changed fields, as the example does.

### 8.3 Trash and restore

- No dedicated trash API is documented. Soft-deleted items are listed with `GET .../v2/documents?filter[isDeleted]=true` and `GET .../v2/folders?filter[isDeleted]=true`. They carry `is-deleted: true` and `deleted-date` (`api/documents.md:83`, `:1472`, `:395-397`, `:1690-1692`).
- Retention and purge of soft-deleted items are **[UNDOCUMENTED]**.

---

## 9. Locks, check-out, and web URLs

### 9.1 Locks / check-out

**[UNDOCUMENTED / absent]** No lock, check-out, check-in or lease concept appears anywhere in the docs. Conflicts must be detected client-side, for example by comparing the server `revision` with the last-synced `revision` before uploading.

### 9.2 Open in browser

| Target | URL | Source / notes |
|---|---|---|
| Project | `attributes.project-uri`, for example `https://{region}.streambim.com/webapp/default/#/?projectId={projectId}` | `api/projects.md:371`, `:882`. The `:371` example has no `https://`; prefix one if missing. |
| Document viewer | `https://{region}.streambim.com/project-{projectId}/api/v1/documents/{documentId}/_show?revision={rev}&microtoken={microtoken}` | `api/tileserver.md:83-107`. A browser-redirect endpoint. It needs a **fresh microtoken** (§6.3), so build it when the user clicks, never store it. `revision` is optional. |
| 3D viewer | `https://{region}.streambim.com/webapp/default/#/viewer?projectId={id}[&buildingId={id}]` | `api/widget-api.md:285-300`, `AGENTS.md:56`. `embedded=true` is only for iframes. |
| Folder, or a document in the web app's document list | **[UNDOCUMENTED]** | No SPA route for documents or folders is documented |

**[INFERRED]** The microtoken in a `_show` URL is a credential. Don't log it.

---

## 10. Rate limits, errors, pagination

### 10.1 Rate limits [DOC `README.md:76-83`]

- No numbers, no `429` and no `Retry-After` are documented.
- The backend is "optimized for loads matching the StreamBIM app". Be careful with "overriding default pagination limits", "high-frequency polling" and "large batch operations". "Misuse may result in client blocking. Contact `cto@rendra.io`."
- Client logging is rate-limited (`api/infrastructure.md:126`).
- **Recommendation:** poll at most about every 30 s, which matches the web app (`api/sync.md:282`). Use moderate page sizes (for example 200–500), and run downloads and uploads with small concurrency. Back off exponentially on `429`/`5xx` even though they aren't documented.

### 10.2 Error format (JSON:API endpoints)

```json
{
  "errors": [
    {
      "status": "404",
      "code": "notFound",
      "detail": "Invalid label ID",
      "meta": { "params": { "innerError": "Invalid label ID" } }
    }
  ]
}
```

Source: `api/documents.md:1024-1039`, `:1255-1270`, `:1325-1340`.

- `status` is a **string**.
- `code` is a camelCase token. Values seen: `notFound`, `topicDoesNotExist`, `internalServerError`, `conflictError` (`api/topics.md:231-236`, `:392-399`, `:1142-1155`).
- `meta.params` carries the details.

Documented HTTP statuses:

| Code | Meaning in docs | Source |
|---|---|---|
| 400 | Bad URL/query/filter format, or a forbidden filter combination | `api/projects.md:57`, `api/documents.md:69`, `api/users.md:774` |
| 401 | "Not authorized. Log in via POST /auth/v1/login". `_starttrigger` returns 401 for no access. | `api/users.md:114`, `api/projects.md:546` |
| 403 | Insufficient privileges / no access to project | `api/projects.md:58`, `api/users.md:115` |
| 404 | Not found. `/mgw/api/v3/users/{id}` also uses it for "insufficient privileges". | `api/users.md:825` |
| 409 | Conflict, for example a duplicate label name | `api/topics.md:1139-1155` |
| 500 | Server error. Wrong IDs can produce 500 `"sql: no rows in result set"` instead of 404. | `api/topics.md:388-399`, `api/projects.md:818` |

- Error bodies for `/auth/v1/*` are **[UNDOCUMENTED]**.
- **[LIVE]** An unauthenticated `GET https://app.streambim.com/api/v1/ping` returned `302 text/html`, a redirect rather than JSON 401. Expect HTML redirects on some paths when auth is missing; check the `Content-Type` before parsing JSON.

### 10.3 Pagination and filter conventions

- Limit/offset paging: `?page[skip]=0&page[limit]=10`. `skip` defaults to `0`; `limit` defaults to **all** (`null`) (`README.md:55-64`, `api/documents.md:32-39`).
- List endpoints return `meta.total` (`api/documents.md:140`, `api/topics.md:45-51`). Page until `skip >= total`.
- Paging is documented on `/v2/documents`. It isn't documented on folders, revisions or `/mgw` project lists.
- Filters use deep-object syntax, `?filter[name]=value` (`README.md:66-74`). Array encoding is inconsistent across the docs: comma-separated `pdf,ifc` (`api/documents.md:160`), and JSON arrays `["1025"]` / `["GROUPID_1"]` (`api/ifc-files.md:198`, `api/users.md:101`). URL-encode the brackets, as in `filter%5Bprofile%5D=true` (`api/users.md:1366`).
- Batch requests: `POST /project-{projectId}/api/v1/batch` in "ember-batch-request" format. The format isn't documented further (`api/infrastructure.md:81-102`).

---

## 11. Other things a sync client should know

1. **StreamBIM can itself sync from Interaxo or SharePoint into the document library.**
   - Triggered with `POST /project-{id}/api/v1/sync`; status via `GET /project-{id}/api/v1/sync` and `GET .../v2/sync-statuses` (`api/sync.md`).
   - Projects with the `INTERAXO` feature (`api/projects.md:1048`) or `INTERAXO_*` configs may have documents that are mirrors managed by that integration.
   - **[INFERRED]** Uploads or edits from a third-party client could conflict with, or be overwritten by, that server-side sync.
   - `external-id` on documents and folders is "Custom ID used by integration partners" (`api/documents.md:85`). Could the sync client set it to tag its own objects? Allowed values and semantics are **[UNDOCUMENTED]**.
2. **Security note in the docs:** `GET /v2/configs` returns `INTERAXO_REFRESH_TOKEN` in plain text to anyone with config read access (`api/sync.md:292`).
3. **IFC models:** uploading an `.ifc` document doesn't make it appear in the 3D model. That needs `POST .../v2/ifc-files` and a converter job, with Project Admin rights (`api/ifc-files.md`, `api/converter-jobs.md`). A file-sync client can ignore this.
4. **Labels:** `GET .../v2/document-labels` and `GET .../v2/label-groups` (`api/documents.md:791-1365`). These are optional metadata.

---

## 12. Contradictions and gaps (checklist for live testing)

| # | Topic | Issue | Where |
|---|---|---|---|
| 1 | Login | `session` isn't listed in the login response but is needed for MFA. **[WEBAPP]** It is in the body of the **401** that carries `CHALLENGE_REQUESTED` (§2.2); **[LIVE]** confirmed 2026-11-12 | `api/authentication.md:27-37` vs `:230` |
| 2 | MFA | `mfa/verify` response doesn't list `refreshToken`. **[WEBAPP]** The web app keeps one after MFA (§2.3) | `api/authentication.md:235-243` |
| 3 | Refresh | Takes the *expired* idToken; proactive refresh untested; refresh-token lifetime unknown | `api/authentication.md:54-78` |
| 4 | SSO | Callback `/auth/v1/federation` and token hand-off undocumented; redirect URI is fixed to StreamBIM | `api/authentication.md:141-158`, §2.6 [LIVE] |
| 5 | BYO client id / PKCE / device code | Not documented | — |
| 6 | Logout / revoke | Not documented | — |
| 7 | Regions | Docs list `staging`/`dev` and omit `spf`; live JSON misspells `disctintiveLanguages` | `README.md:11-18`, §1.2 [LIVE] |
| 8 | Current user | `user-profiles` response schema undocumented | `api/users.md:1341-1367` |
| 9 | Projects | Unclear whether `/mgw/api/v3/projects` is user-scoped; integer vs string ids | `api/projects.md:304-306`, `:358` |
| 10 | Documents filter | `filter[labelIds]` vs `filter[labels]`; array encoding | `api/documents.md:75`, `api/ifc-files.md:198` |
| 11 | Delta | Exact `since` semantics (renames, moves, deletes, cascades) | `api/documents.md:84`, `:1474` |
| 12 | Checksums | None exposed | `api/documents.md:382-411` |
| 13 | Download link | JSON `{data: "/documents/{id}/{rev}/download?token=..."}` vs plain text `documents/{id}/download/{filename}` | `api/documents.md:439-451` vs `:1726-1742` |
| 14 | Download | Redirect to S3? Range/resume? Older revisions? | §6 |
| 15 | Upload ticket | Request `type` is `"attachments"`; attribute is `file-size`, not `filesize` | `api/attachments.md:263-279` |
| 16 | New revision | Example sends `upload-ticket: null`, and the table marks it optional | `api/documents.md:600`, `:618` |
| 17 | Multipart | `signer-url` `/aws/_uploadsigner` protocol undocumented; size limits undocumented | `api/attachments.md:289-299` |
| 18 | `_upload` form | Unclear whether it also creates the document | `api/attachments.md:1049-1085` |
| 19 | Create responses | Response bodies for POST documents and POST document-revisions undocumented | `api/documents.md:213-279`, `:578-623` |
| 20 | Folder delete | Only soft delete via PATCH; cascade unknown | `api/documents.md:1617-1677` |
| 21 | Document DELETE | Hard vs soft unknown | `api/documents.md:349-378` |
| 22 | PATCH semantics | Partial vs full replacement unclear | `api/documents.md:287` vs `:306-316` |
| 23 | Duplicate names | Same filename in one folder allowed? | — |
| 24 | Export JSON | Two incompatible query and response schemas | `api/documents.md:1760-1838` vs `api/exports.md:245-331` |
| 25 | Revision file id | `RevisionType` vs revision number in the composite id description | `api/documents.md:406`, `:409` |
| 26 | Rate limits | No numbers, no 429 contract | `README.md:76-83` |
| 27 | Locks | None | — |
| 28 | Document web URL | Only `_show` with a microtoken; no SPA deep link | `api/tileserver.md:83-107` |

---

## 13. Suggested mapping to a sync engine (recommendation, not documentation)

| Sync concept | StreamBIM mapping |
|---|---|
| Account | `{region subdomain, username(email)}`, plus `refreshToken` in the keychain |
| Remote root | Project (`/mgw/api/v3/project-links` id) → folder `0` |
| Item id | Folder `id` / document `id` (strings) |
| Parent | `relationships.parent.data.id` |
| Name | Folder `attributes.name` / document `attributes.filename` |
| Version / etag substitute | Document `attributes.revision` (int), with `filesize` and `last-modified` as tie-breakers. No content hash exists. |
| Modified time | `attributes.last-modified` (ISO-8601, microseconds, UTC) |
| Size | `attributes.filesize` |
| Modified by | Documents: `uploaded-by` (email). Folders: `modified-by` (email). |
| Delta cursor | `meta.timestamp` (epoch ns) → next `filter[since]` |
| Tombstones | `filter[isDeleted]=true` with `filter[since]` |
| Download | `downloadlink` → GET (§6) |
| Upload new | ticket → S3 PUT → POST documents (§7.6) |
| Upload change | ticket → S3 PUT → POST document-revisions `{id}-{rev+1}` (§7.7). Before uploading, re-GET the document and abort as a conflict if `revision` moved. |
| Rename / move / delete | PATCH with `filename`/`name`, `parent`, `is-deleted` (§8) |
| Read-only detection | `is-hibernated` project; BCF `project_actions` lacking `createDocument`; folder `access-level` (semantics unverified); 403 on write |
| Open in browser | Document: `_show` + fresh microtoken. Project: `project-uri`. |
