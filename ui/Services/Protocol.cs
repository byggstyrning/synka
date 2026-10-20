using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Synka.Services;

// The messages of docs\IPC.md. One record per engine message, with the field
// names of the protocol: the snake_case naming policy below maps `SignedIn` to
// `signed_in` and so on. A field the engine leaves out simply arrives as its
// default — every list is therefore read as possibly null.

/// <summary>The signed-in account, or null when signed out.</summary>
public sealed record UserInfo(string? Name, string? Email);

/// <summary>An upload or download in progress, in bytes.</summary>
public sealed record TransferInfo(string? Name, string? Project, string? Direction, long Done, long Total);

/// <summary>
/// A burst of local deletes the engine holds back until the user confirms it
/// (mass-delete protection). Answered with `confirm_deletes`.
/// </summary>
public sealed record HeldDeleteInfo(string? ProjectId, string? Project, long Count);

/// <summary>
/// One document provider's account (docs\IPC.md, `status.accounts`). Provider is
/// `acc`, `streambim` or `interaxo`. Configured is false when the provider cannot
/// sign in yet (no client id). Host is the StreamBIM region host, or null.
/// </summary>
public sealed record AccountInfo(
    string Provider,
    string Label,
    bool Configured,
    bool SignedIn,
    bool SigningIn,
    UserInfo? User,
    string? Host);

/// <summary>
/// The engine's own picture of the sync. Pushed on every change, throttled to 2/s.
/// The job counts are 64-bit on the engine side (i64), so they are read as long.
/// <c>held_deletes</c> is newer than the rest; an engine without it leaves it out.
/// <c>accounts</c> is newer still: an older engine leaves it out (null), and the window
/// then reads the ACC account from <c>signed_in</c> and <c>user</c>.
/// </summary>
public sealed record StatusInfo(
    bool SignedIn,
    UserInfo? User,
    string? State,
    string? Text,
    string? Error,
    long PendingJobs,
    long FailedJobs,
    List<TransferInfo>? Transfers,
    string? SyncRoot,
    long? LocalBytes,
    string? EngineVersion,
    List<HeldDeleteInfo>? HeldDeletes,
    IReadOnlyList<AccountInfo>? Accounts = null);

/// <summary>One project in the hub tree.</summary>
public sealed record ProjectInfo(string Id, string Name, bool Subscribed, string? LocalPath);

/// <summary>
/// A hub (ACC account, StreamBIM, Interaxo community) and its projects.
/// <c>provider</c> is newer than the rest: null from an older engine means ACC.
/// </summary>
public sealed record HubInfo(string Id, string Name, List<ProjectInfo>? Projects, string? Provider = null);

/// <summary>The hub tree. `loading` while the engine asks ACC.</summary>
public sealed record ProjectsInfo(bool Loading, List<HubInfo>? Hubs);

/// <summary>
/// Current settings, as the engine holds them. <c>auto_update</c> is newer than
/// the rest: an engine that does not know it leaves it out, and it arrives as null.
/// The window shows its switch only when the engine sends it. The APS override editor
/// is also capability-gated so older engines never show a setting they cannot apply.
/// <c>update_channel_configured</c> is false when no update channel is set (the default):
/// the engine never checks for updates, and the window hides the update controls.
/// The provider fields (<c>providers</c> and the Interaxo settings) are newest: an older
/// engine leaves them out. Interaxo settings can change only while Interaxo is signed out.
/// </summary>
public sealed record SettingsInfo(
    bool PropagateDeletes,
    int MaxDownloads,
    int MaxUploads,
    int PollActiveSecs,
    string? LogLevel,
    bool StartAtLogin,
    string? SyncRoot,
    bool ClientIdConfigured,
    bool? AutoUpdate,
    string? ApsClientId,
    bool? ApsClientIdOverrideSupported,
    bool? ApsClientIdValidationSupported,
    bool? UpdateChannelConfigured,
    IReadOnlyList<string>? Providers = null,
    string? InteraxoClientId = null,
    string? InteraxoRedirectUri = null,
    string? InteraxoRedirectUriDefault = null,
    bool InteraxoClientSecretSet = false);

/// <summary>
/// Progress of a sign-in (docs\IPC.md, `sign_in_progress`). State is `browser`,
/// `mfa_required` (StreamBIM: send a code), `signed_in`, `failed` (Error set) or `cancelled`.
/// </summary>
public sealed record SignInProgressInfo(string Provider, string State, string? Error);

/// <summary>One StreamBIM region: the subdomain is the region id sent with `sign_in`.</summary>
public sealed record StreamBimRegion(string Subdomain, string Title, bool IsDefault);

/// <summary>The StreamBIM regions the engine offers (live list, or its built-in fallback).</summary>
public sealed record StreamBimRegionsInfo(IReadOnlyList<StreamBimRegion>? Regions);

/// <summary>Correlated result of testing and saving a candidate APS public client.</summary>
public sealed record ApsClientIdValidationInfo(string? RequestId, string? State, string? Error);

/// <summary>
/// One line of the activity list. <c>kind</c> is read as an open set: kinds the
/// window does not know (newer engines add some) are shown as a plain event.
/// </summary>
public sealed record ActivityItem(long TsMs, string? Kind, string? Name, string? Project, string? Detail);

/// <summary>The whole activity list, newest first, as sent on `hello`.</summary>
public sealed record ActivityList(List<ActivityItem>? Items);

/// <summary>
/// Result of the update check. <c>installing</c> is newer than the rest: true while
/// the engine installs an update (on request, or by itself when idle). Absent means false.
/// </summary>
public sealed record UpdateInfo(bool Available, string? Version, string? Error, bool? Installing);

/// <summary>A message for the user from the engine. Severity: info, warning or error.</summary>
public sealed record NotifyInfo(string? Title, string? Body, string? Severity);

/// <summary>Answer to a command that carried an id. Id is null for an unsolicited ack.</summary>
public sealed record Ack(long? Id, bool Ok, string? Error)
{
    /// <summary>Local metadata: distinguishes a received refusal from transport uncertainty.</summary>
    [JsonIgnore]
    public bool IsEngineResponse { get; init; }
}

/// <summary>
/// The source-generated serializer for the protocol. Snake_case names, as in
/// docs\IPC.md. Every type that crosses the pipe is listed here.
/// </summary>
[JsonSourceGenerationOptions(
    PropertyNamingPolicy = JsonKnownNamingPolicy.SnakeCaseLower,
    DefaultIgnoreCondition = JsonIgnoreCondition.Never)]
[JsonSerializable(typeof(UserInfo))]
[JsonSerializable(typeof(TransferInfo))]
[JsonSerializable(typeof(HeldDeleteInfo))]
[JsonSerializable(typeof(StatusInfo))]
[JsonSerializable(typeof(ProjectInfo))]
[JsonSerializable(typeof(HubInfo))]
[JsonSerializable(typeof(ProjectsInfo))]
[JsonSerializable(typeof(SettingsInfo))]
[JsonSerializable(typeof(ApsClientIdValidationInfo))]
[JsonSerializable(typeof(AccountInfo))]
[JsonSerializable(typeof(SignInProgressInfo))]
[JsonSerializable(typeof(StreamBimRegion))]
[JsonSerializable(typeof(StreamBimRegionsInfo))]
[JsonSerializable(typeof(ActivityItem))]
[JsonSerializable(typeof(ActivityList))]
[JsonSerializable(typeof(UpdateInfo))]
[JsonSerializable(typeof(NotifyInfo))]
[JsonSerializable(typeof(Ack))]
internal partial class EngineJsonContext : JsonSerializerContext
{
}
