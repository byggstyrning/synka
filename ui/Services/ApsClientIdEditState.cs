using System;

namespace Synka.Services;

/// <summary>
/// Keeps the draft separate from saved settings. An enqueue ACK does not confirm a
/// test or save. Same-value tests require their own correlated completion event.
/// </summary>
internal sealed class ApsClientIdEditState
{
    private int _requestVersion;
    private string _beforeRequest = string.Empty;

    public bool Initialized { get; private set; }
    public string Confirmed { get; private set; } = string.Empty;
    public string Draft { get; private set; } = string.Empty;
    public string? PendingValue { get; private set; }
    public string? RequestId { get; private set; }
    public string? Problem { get; private set; }
    public bool Saved { get; private set; }
    public bool IsWaiting => PendingValue is not null;
    public bool IsTesting => IsWaiting && RequestId is not null;
    public int RequestVersion => _requestVersion;
    public bool HasChanges => Normalize(Draft) != Confirmed;
    public bool CanSave => Initialized && !IsWaiting && Normalize(Draft).Length > 0;
    public bool CanUseDefault => Initialized && !IsWaiting && (Confirmed.Length > 0 || Normalize(Draft).Length > 0);

    public static string Normalize(string? value) => value?.Trim() ?? string.Empty;

    public void Edit(string value)
    {
        if (IsWaiting) return;
        Draft = value;
        Problem = null;
        Saved = false;
    }

    public void ObserveSettings(string? value)
    {
        var preserveDraft = Initialized && !string.Equals(Draft, Confirmed, StringComparison.Ordinal);
        Confirmed = Normalize(value);
        // A changed, saved value can recover a lost completion during engine restart.
        // An unchanged snapshot can never prove that a same-ID browser test succeeded.
        if (IsWaiting && string.Equals(PendingValue, Confirmed, StringComparison.Ordinal)
            && (!IsTesting || Confirmed != _beforeRequest))
        {
            CompleteSaved();
        }
        else if (!Initialized || !preserveDraft)
        {
            Draft = Confirmed;
        }
        Initialized = true;
    }

    public int Begin(string value, bool validate = false)
    {
        if (IsWaiting) throw new InvalidOperationException("An APS setting change is already pending.");
        _beforeRequest = Confirmed;
        PendingValue = Normalize(value);
        RequestId = validate ? Guid.NewGuid().ToString("D") : null;
        Problem = null;
        Saved = false;
        return ++_requestVersion;
    }

    public bool ObserveValidation(ApsClientIdValidationInfo result)
    {
        if (!IsTesting || !string.Equals(RequestId, result.RequestId, StringComparison.Ordinal)) return false;
        switch (result.State)
        {
            case "testing": return true;
            case "saved":
                Confirmed = PendingValue!;
                CompleteSaved();
                return true;
            case "failed":
                return FailIfPending(_requestVersion, result.Error ?? Lang.T("APS-testet misslyckades.", "The APS test failed."));
            case "cancelled":
                return FailIfPending(_requestVersion, Lang.T("Testet avbröts. Tidigare inställning och ditt utkast behålls.", "Test cancelled. The previous setting and your draft are kept."));
            default: return false;
        }
    }

    private void CompleteSaved()
    {
        PendingValue = null;
        RequestId = null;
        Draft = Confirmed;
        Problem = null;
        Saved = true;
    }

    /// <summary>A lost ACK may mean the engine restarted after saving; keep waiting for confirmation.</summary>
    public bool ObserveAck(int version, Ack ack) =>
        !ack.Ok && ack.IsEngineResponse && FailIfPending(version, ack.Error);

    public bool IsPending(int version) => IsWaiting && version == _requestVersion;

    public bool FailIfPending(int version, string? problem)
    {
        if (!IsPending(version)) return false;
        PendingValue = null;
        RequestId = null;
        Problem = problem;
        Saved = false;
        return true;
    }
}
