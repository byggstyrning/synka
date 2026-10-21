using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Linq;
using System.Threading.Tasks;
using CommunityToolkit.WinUI.Controls;
using Synka.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace Synka.Views;

/// <summary>
/// A transfer's progress across status messages. Keeping it by key allows a
/// smoothed speed without recreating any XAML elements on each update.
/// </summary>
public sealed class TransferRow
{
    /// <summary>A speed is shown only after this much measuring, so the first jumpy samples are not.</summary>
    private const long SpeedAfterMs = 1500;

    private long _lastDone = -1;
    private long _lastAt;
    private long _measuringSince;
    private double _speed;

    public TransferRow(string key, TransferInfo transfer)
    {
        Key = key;
        Name = string.IsNullOrEmpty(transfer.Name) ? "–" : transfer.Name;
        Project = transfer.Project ?? string.Empty;
        FileTypeLabel = Fmt.FileTypeLabel(transfer.Name);
        FileTypeGlyph = Fmt.FileTypeGlyph(transfer.Name);
        FileTypeDescription = Fmt.FileTypeDescription(transfer.Name);
        Glyph = Fmt.DirectionGlyph(transfer.Direction);
        DirectionLabel = Fmt.DirectionLabel(transfer.Direction);
        Update(transfer);
    }

    public string Key { get; }

    public string Glyph { get; }

    public string FileTypeLabel { get; }

    public string FileTypeGlyph { get; }

    public string FileTypeDescription { get; }

    public string DirectionLabel { get; }

    public string Name { get; }

    public string Project { get; }

    public string Subtitle { get; private set; } = string.Empty;

    /// <summary>Per cent done, 0–100.</summary>
    public double Value { get; private set; }

    /// <summary>True while the size is not known.</summary>
    public bool Indeterminate { get; private set; }

    /// <summary>Bytes per second, smoothed. Zero until measured.</summary>
    public double Speed => Environment.TickCount64 - _measuringSince >= SpeedAfterMs ? _speed : 0;

    public void Update(TransferInfo transfer)
    {
        var now = Environment.TickCount64;
        var done = Math.Max(0, transfer.Done);
        var total = transfer.Total;

        if (_lastDone < 0 || done < _lastDone)
        {
            // First sample, or the transfer started over.
            _lastDone = done;
            _lastAt = now;
            _measuringSince = now;
            _speed = 0;
        }
        else if (now - _lastAt >= 200)
        {
            var rate = (done - _lastDone) * 1000.0 / (now - _lastAt);
            _speed = _speed <= 0 ? rate : 0.3 * rate + 0.7 * _speed;
            _lastDone = done;
            _lastAt = now;
        }

        var parts = new List<string>(4);
        if (Project.Length > 0)
        {
            parts.Add(Project);
        }

        parts.Add(Fmt.Pair(done, total));
        if (total > 0)
        {
            parts.Add(Fmt.Percent(done, total));
        }

        if (Speed > 0)
        {
            parts.Add(Fmt.Speed(Speed));
        }

        Subtitle = string.Join(" · ", parts);
        Indeterminate = total <= 0;
        Value = total > 0 ? Math.Clamp(done * 100.0 / total, 0, 100) : 0;
    }

    /// <summary>What a screen reader says for the row.</summary>
    public override string ToString() => $"{DirectionLabel} {Name}, {Subtitle}";
}

/// <summary>A row in the Overview's list of synced projects.</summary>
public sealed record SyncedProjectRow(string Name, string Hub, string? LocalPath, bool Connected)
{
    public bool HasPath => !string.IsNullOrEmpty(LocalPath);

    public bool CanOpen => HasPath && Connected;

    public string OpenLabel => Lang.T($"Öppna mappen för {Name}", $"Open the folder of {Name}");

    public override string ToString() => $"{Name}, {Hub}";
}

public sealed partial class OverviewPage : Page
{
    /// <summary>
    /// The engine's one-liners that only repeat the state the headline already says.
    /// They are left out, so the hero does not say "Synced" twice (once in Swedish).
    /// </summary>
    private static readonly HashSet<string> EchoTexts = new(StringComparer.OrdinalIgnoreCase)
    {
        "synkad", "synkar", "offline", "logga in", "inte inloggad", "utloggad",
    };

    private static readonly TimeSpan TransferHold = TimeSpan.FromMilliseconds(1200);

    private Dictionary<string, TransferRow> _transferRows = new();
    private readonly DispatcherQueueTimer _transferHoldTimer;
    private string? _displayedTransferKey;
    private bool _holdingLastTransfer;

    private bool _attached;

    /// <summary>The sign-in hint as the XAML has it, for when the account changes again.</summary>
    private readonly string _signInHintDefault;

    public OverviewPage()
    {
        InitializeComponent();
        _transferHoldTimer = DispatcherQueue.CreateTimer();
        _transferHoldTimer.Interval = TransferHold;
        _transferHoldTimer.IsRepeating = false;
        _transferHoldTimer.Tick += OnTransferHoldElapsed;
        _signInHintDefault = SignInHint.Text;
    }

    private static EngineClient Engine => EngineClient.Instance;

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        if (_attached)
        {
            return;
        }
        _attached = true;

        Engine.PropertyChanged += OnEnginePropertyChanged;
        RefreshStatus();
        RefreshProjects();
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        Engine.PropertyChanged -= OnEnginePropertyChanged;
        _transferHoldTimer.Stop();
        _holdingLastTransfer = false;
    }

    private void OnEnginePropertyChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(EngineClient.Status):
            case nameof(EngineClient.ConnectionState):
            case nameof(EngineClient.IsSignedIn):
                RefreshStatus();
                break;

            case nameof(EngineClient.IsConnected):
                RefreshStatus();
                RefreshProjects();
                break;

            case nameof(EngineClient.Projects):
                RefreshProjects();
                break;

            case nameof(EngineClient.IsFreeingUp):
                RefreshFreeUpControl();
                break;
        }
    }

    // --------------------------------------------------------------- status

    /// <summary>Everything that comes from `status`. Runs up to twice a second, so it changes only what it must.</summary>
    private void RefreshStatus()
    {
        var connected = Engine.IsConnected;
        var status = connected ? Engine.Status : null;
        var signedIn = Engine.IsSignedIn;

        var explanation = ApplyState(status);

        // Routine progress is already in the transfer and queue cards. Showing its
        // changing one-liner here adds and removes a line in the hero every time a
        // quick transfer starts or finishes.
        var text = StatusLine(status);
        var error = status?.Error;
        var echo = EchoTexts.Contains(text.TrimEnd('…', '.', ' '))
            || (!string.IsNullOrEmpty(error) && text.Contains(error, StringComparison.Ordinal));
        var routineSync = status?.State is "synced" or "syncing" or "waiting" or "attention";
        SetText(StatusText, explanation ?? (routineSync || echo ? string.Empty : text));
        SetVisibility(StatusText, StatusText.Text.Length == 0 ? Visibility.Collapsed : Visibility.Visible);

        SetText(AccountText, AccountSummary(status));
        SetVisibility(AccountText, AccountText.Text.Length == 0 ? Visibility.Collapsed : Visibility.Visible);

        SetText(ErrorText, ErrorDisplay.FriendlyDetail(status?.Error));
        SetVisibility(ErrorText, ErrorText.Text.Length == 0 ? Visibility.Collapsed : Visibility.Visible);

        // Buttons: the folder needs the engine, a sync needs an account. Without an
        // engine the one useful action is to start it.
        var canStart = !connected && Engine.ConnectionState is EngineConnectionState.Disconnected or EngineConnectionState.NotRunning;
        OpenFolderButton.Visibility = connected ? Visibility.Visible : Visibility.Collapsed;
        SyncNowButton.Visibility = connected ? Visibility.Visible : Visibility.Collapsed;
        SyncNowButton.IsEnabled = signedIn;
        StartEngineButton.Visibility = canStart ? Visibility.Visible : Visibility.Collapsed;

        // Shown while nothing is signed in (ACC is then the account to sign in). Another
        // provider signed in: the card stays hidden, and the Konton page has the rest.
        var noAccount = connected && status is { SignedIn: false };
        SignInCard.Visibility = noAccount ? Visibility.Visible : Visibility.Collapsed;
        SignInSettingsButton.Visibility = noAccount ? Visibility.Visible : Visibility.Collapsed;
        var settingsText = SignInSettingsText();
        if (SignInSettingsButton.Content as string != settingsText)
        {
            SignInSettingsButton.Content = settingsText;
        }
        if (signedIn)
        {
            SignInHint.Text = _signInHintDefault;
        }

        RefreshTransfers(status?.Transfers);
        RefreshQueue(status);
        RefreshStorage(status);
    }

    /// <summary>
    /// The line on the sign-in card's button: the Konton providers the engine offers (settings.providers).
    /// Without Interaxo it names ACC and StreamBIM only.
    /// </summary>
    private static string SignInSettingsText() => Engine.ProviderOffered(ProviderNames.Interaxo)
        ? Lang.T("Logga in på StreamBIM eller Interaxo under Konton.", "Sign in to StreamBIM or Interaxo under Accounts.")
        : Lang.T("Logga in på ACC eller StreamBIM under Konton.", "Sign in to ACC or StreamBIM under Accounts.");

    /// <summary>The engine's one-liner while any sign-in waits for a browser. It is generic: StreamBIM has no browser.</summary>
    private const string BrowserWaitText = "Väntar på inloggning i webbläsaren…";

    /// <summary>
    /// The engine's one-liner. When StreamBIM is the only sign-in pending, the browser wait is
    /// not what it is doing: the hero says it signs in to StreamBIM instead.
    /// </summary>
    private static string StatusLine(StatusInfo? status)
    {
        var text = status?.Text?.Trim() ?? string.Empty;
        if (text == BrowserWaitText
            && Engine.StreamBimAccounts.Any(account => account.SigningIn)
            && Engine.AccountFor(ProviderNames.Acc) is not { SigningIn: true }
            && Engine.AccountFor(ProviderNames.Interaxo) is not { SigningIn: true })
        {
            return Lang.T("Loggar in på StreamBIM…", "Signing in to StreamBIM…");
        }

        return text;
    }

    /// <summary>
    /// Who is signed in. One account: "Inloggad som …". Several (ACC and StreamBIM, say): each
    /// with its name, "ACC: … · StreamBIM Japan: …". Each StreamBIM server is its own account, named
    /// as its hub is. An engine without per-provider accounts reports only `status.user`, and that one is shown.
    /// </summary>
    private static string AccountSummary(StatusInfo? status)
    {
        if (!Engine.IsConnected)
        {
            return string.Empty;
        }

        var accounts = Engine.Accounts.Where(account => account is { SignedIn: true }).ToList();
        if (accounts.Count > 1)
        {
            return string.Join(" · ", accounts.Select(account =>
            {
                var label = ProviderNames.AccountLabel(account, Engine.StreamBimRegions);
                var who = ProviderNames.Who(account.User);
                return who.Length == 0 ? label : $"{label}: {who}";
            }));
        }

        var user = accounts.Count == 1
            ? accounts[0].User
            : status is { SignedIn: true } ? status.User : null;
        var single = ProviderNames.Who(user);
        return single.Length == 0 ? string.Empty : Lang.T($"Inloggad som {single}", $"Signed in as {single}");
    }

    private enum Tone
    {
        Neutral,
        Success,
        Caution,
        Critical,
        Busy,
    }

    /// <summary>
    /// The hero's glyph and headline. Not connected: what is being done about the
    /// engine. Returns a line that explains the state when the engine has none to give.
    /// </summary>
    private string? ApplyState(StatusInfo? status)
    {
        Tone tone;
        string glyph;
        string headline;
        string? explanation = null;

        if (!Engine.IsConnected)
        {
            (tone, glyph, headline) = Engine.ConnectionState switch
            {
                EngineConnectionState.Starting => (Tone.Busy, "", Lang.T("Startar motorn …", "Starting the engine …")),
                EngineConnectionState.Restarting => (Tone.Busy, "", Lang.T("Motorn startar om …", "The engine is restarting …")),
                EngineConnectionState.Disconnected => (Tone.Caution, "", Lang.T("Ingen kontakt med motorn", "No contact with the engine")),
                EngineConnectionState.NotRunning => (Tone.Caution, "", Lang.T("Motorn körs inte", "The engine is not running")),
                _ => (Tone.Busy, "", Lang.T("Ansluter till motorn …", "Connecting to the engine …")),
            };

            if (tone == Tone.Caution)
            {
                explanation = Lang.T(
                    "Synken sköts av motorn, som körs i bakgrunden. Starta den för att fortsätta synka.",
                    "The sync is done by the engine, which runs in the background. Start it to keep syncing.");
            }
            else if (Engine.ConnectionState == EngineConnectionState.Restarting)
            {
                explanation = Lang.T(
                    "Synkmappen flyttas. Projekten byggs om på den nya platsen när motorn är tillbaka.",
                    "The sync folder is being moved. The projects are rebuilt in the new place once the engine is back.");
            }
        }
        else
        {
            (tone, glyph, headline) = status?.State switch
            {
                null => (Tone.Busy, "", Lang.T("Hämtar status …", "Fetching the status …")),
                "synced" => (Tone.Success, "", Lang.T("Synkad", "Synced")),
                "syncing" => (Tone.Busy, "", Lang.T("Synkar …", "Syncing …")),
                "waiting" => (Tone.Neutral, "\uE823", Lang.T("Väntar på synk", "Waiting to sync")),
                "attention" => (Tone.Caution, "", Lang.T("Behöver ses över", "Needs attention")),
                "offline" => (Tone.Caution, "", Lang.T("Offline", "Offline")),
                "error" => (Tone.Critical, "", Lang.T("Fel", "Error")),
                "signed_out" => (Tone.Neutral, "", Lang.T("Inte inloggad", "Not signed in")),
                _ => (Tone.Neutral, "", Lang.T("Okänd status", "Unknown status")),
            };
        }

        var busy = tone == Tone.Busy;
        if (StateSpinner.IsActive != busy)
        {
            StateSpinner.IsActive = busy;
        }
        SetVisibility(StateSpinner, busy ? Visibility.Visible : Visibility.Collapsed);

        ShowGlyph(NeutralGlyph, tone == Tone.Neutral, glyph);
        ShowGlyph(SuccessGlyph, tone == Tone.Success, glyph);
        ShowGlyph(CautionGlyph, tone == Tone.Caution, glyph);
        ShowGlyph(CriticalGlyph, tone == Tone.Critical, glyph);

        AutomationProperties.SetName(StateIcon, headline);
        SetText(HeadlineText, headline);
        return explanation;
    }

    private static void ShowGlyph(FontIcon icon, bool on, string glyph)
    {
        if (on)
        {
            if (icon.Glyph != glyph) icon.Glyph = glyph;
        }
        SetVisibility(icon, on ? Visibility.Visible : Visibility.Collapsed);
    }

    // ------------------------------------------------------------ transfers

    /// <summary>
    /// Keeps speed measurements for every active transfer, but shows one stable,
    /// compact row. The heading reports the total count. A short hold after the
    /// last transfer prevents quick jobs from flashing the empty state.
    /// </summary>
    private void RefreshTransfers(List<TransferInfo>? transfers)
    {
        var next = new Dictionary<string, TransferRow>();
        var desired = new List<TransferRow>();
        var seen = new Dictionary<string, int>();

        foreach (var transfer in transfers ?? new List<TransferInfo>())
        {
            if (transfer is null)
            {
                continue;
            }

            var baseKey = $"{transfer.Direction}|{transfer.Project}|{transfer.Name}";
            seen[baseKey] = seen.TryGetValue(baseKey, out var count) ? count + 1 : 1;
            var key = seen[baseKey] == 1 ? baseKey : $"{baseKey}#{seen[baseKey]}";

            if (_transferRows.TryGetValue(key, out var row))
            {
                row.Update(transfer);
            }
            else
            {
                row = new TransferRow(key, transfer);
            }

            next[key] = row;
            desired.Add(row);
        }

        _transferRows = next;

        if (desired.Count == 0)
        {
            if (Engine.IsConnected && _displayedTransferKey is not null)
            {
                if (!_holdingLastTransfer)
                {
                    _holdingLastTransfer = true;
                    SetText(TransfersSummary, Lang.T("Senaste", "Recent"));
                    _transferHoldTimer.Start();
                }
            }
            else
            {
                _transferHoldTimer.Stop();
                _holdingLastTransfer = false;
                _displayedTransferKey = null;
                ShowNoTransfers();
            }
            return;
        }

        _transferHoldTimer.Stop();
        _holdingLastTransfer = false;

        var displayedRow = _displayedTransferKey is not null && next.TryGetValue(_displayedTransferKey, out var current)
            ? current
            : desired[0];
        _displayedTransferKey = displayedRow.Key;

        if (TransferGlyph.Glyph != displayedRow.Glyph) TransferGlyph.Glyph = displayedRow.Glyph;
        AutomationProperties.SetName(TransferGlyph, displayedRow.DirectionLabel);
        SetText(TransferName, displayedRow.Name);
        SetText(TransferTypeLabel, displayedRow.FileTypeLabel);
        if (TransferTypeGlyph.Glyph != displayedRow.FileTypeGlyph) TransferTypeGlyph.Glyph = displayedRow.FileTypeGlyph;
        if (AutomationProperties.GetName(TransferTypeGlyph) != displayedRow.FileTypeDescription)
        {
            AutomationProperties.SetName(TransferTypeGlyph, displayedRow.FileTypeDescription);
            ToolTipService.SetToolTip(TransferTypeGlyph, displayedRow.FileTypeDescription);
        }
        SetVisibility(TransferTypeBadge, displayedRow.FileTypeLabel.Length == 0 ? Visibility.Collapsed : Visibility.Visible);
        SetText(TransferSubtitle, displayedRow.Subtitle);
        if (TransferProgress.IsIndeterminate != displayedRow.Indeterminate)
        {
            TransferProgress.IsIndeterminate = displayedRow.Indeterminate;
        }
        if (TransferProgress.Value != displayedRow.Value) TransferProgress.Value = displayedRow.Value;
        AutomationProperties.SetName(TransferProgress, displayedRow.Name);

        SetVisibility(NoTransfersText, Visibility.Collapsed);
        SetVisibility(ActiveTransferPanel, Visibility.Visible);

        var countText = Lang.T($"{desired.Count} pågår", $"{desired.Count} in progress");
        SetText(TransfersSummary, countText);
    }

    private void OnTransferHoldElapsed(DispatcherQueueTimer sender, object args)
    {
        _holdingLastTransfer = false;
        _displayedTransferKey = null;
        RefreshTransfers(Engine.IsConnected ? Engine.Status?.Transfers : null);
    }

    private void ShowNoTransfers()
    {
        SetText(TransfersSummary, string.Empty);
        SetVisibility(ActiveTransferPanel, Visibility.Collapsed);
        SetVisibility(NoTransfersText, Visibility.Visible);
    }

    // ---------------------------------------------------------- queue, storage

    private void RefreshQueue(StatusInfo? status)
    {
        if (status is null)
        {
            // No engine, no queue to speak of. Zero would be a claim the window cannot make.
            SetText(QueueCard, Lang.T("Köade ändringar", "Queued changes"), Lang.T(
                "Visas när fönstret har kontakt med motorn.",
                "Shown once the window is in contact with the engine."));
            QueueLogButton.Visibility = Visibility.Collapsed;
            QueueRetryButton.Visibility = Visibility.Collapsed;
            return;
        }

        var pending = status.PendingJobs;
        var failed = status.FailedJobs;

        var header = pending > 0
            ? Lang.T($"{Fmt.Count(pending)} {(pending == 1 ? "ändring" : "ändringar")} i kö",
                $"{Fmt.Count(pending)} {(pending == 1 ? "change" : "changes")} queued")
            : Lang.T("Inga köade ändringar", "No queued changes");

        var description = failed > 0
            ? Lang.T(
                $"{Fmt.Count(failed)} misslyckades. Aktiviteten och loggen säger varför.",
                $"{Fmt.Count(failed)} failed. The activity and the log say why.")
            : Lang.T(
                "Ändringar du gör i synkmappen laddas upp till molnet.",
                "Changes you make in the sync folder are uploaded to the cloud.");

        SetText(QueueCard, header, description);
        QueueLogButton.Visibility = failed > 0 ? Visibility.Visible : Visibility.Collapsed;
        QueueRetryButton.Visibility = failed > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private void RefreshStorage(StatusInfo? status)
    {
        // local_bytes is null until the engine has worked it out (docs\IPC.md, status).
        var header = status?.LocalBytes is long bytes
            ? Lang.T($"Lokalt lagrat: {Fmt.Bytes(bytes)}", $"Stored locally: {Fmt.Bytes(bytes)}")
            : Lang.T("Lokal lagring", "Local storage");
        SetText(StorageCard, header, null);
        RefreshFreeUpControl();
    }

    private void RefreshFreeUpControl()
    {
        var busy = Engine.IsFreeingUp;
        FreeUpButton.IsEnabled = Engine.IsSignedIn && !busy;
        FreeUpSpinner.IsActive = busy;
        FreeUpSpinner.Visibility = busy ? Visibility.Visible : Visibility.Collapsed;
    }

    private static void SetText(TextBlock block, string value)
    {
        if (block.Text != value) block.Text = value;
    }

    private static void SetVisibility(UIElement element, Visibility value)
    {
        if (element.Visibility != value) element.Visibility = value;
    }

    /// <summary>
    /// Sets a card's texts only when they differ. Status arrives twice a second, and
    /// a new Header object makes the card build its header content again.
    /// </summary>
    private static void SetText(SettingsCard card, string header, string? description)
    {
        if (!Equals(card.Header, header))
        {
            card.Header = header;
        }

        if (description is not null && !Equals(card.Description, description))
        {
            card.Description = description;
        }
    }

    // -------------------------------------------------------------- projects

    private void RefreshProjects()
    {
        var connected = Engine.IsConnected;
        var synced = (Engine.Projects?.Hubs ?? new List<HubInfo>())
            .Where(h => h is not null)
            .SelectMany(h => (h.Projects ?? new List<ProjectInfo>())
                .Where(p => p is { Subscribed: true })
                .Select(p => new SyncedProjectRow(p.Name, h.Name, p.LocalPath, connected)))
            .ToList();

        SyncedList.ItemsSource = synced;
        SyncedList.Visibility = synced.Count == 0 ? Visibility.Collapsed : Visibility.Visible;
        NoProjectsPanel.Visibility = synced.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    // --------------------------------------------------------------- actions

    private async void OnOpenFolder(object sender, RoutedEventArgs e)
    {
        Report(await Engine.OpenFolderAsync(), Lang.T("Mappen gick inte att öppna", "The folder could not be opened"));
    }

    private async void OnSyncNow(object sender, RoutedEventArgs e)
    {
        Report(await Engine.SyncNowAsync(), Lang.T("Synken gick inte att starta", "The sync could not be started"));
    }

    private void OnStartEngine(object sender, RoutedEventArgs e)
    {
        Engine.StartEngine();
    }

    private async void OnSignIn(object sender, RoutedEventArgs e)
    {
        var ack = await Engine.SignInAsync();
        if (ack.Ok)
        {
            // The answer comes as a `status` once the browser is done. The button stays
            // usable: a closed browser tab is retried with one more click.
            SignInHint.Text = Lang.T(
                "Slutför inloggningen i webbläsaren. Om sidan visar ett fel eller stängs, klicka på Logga in igen för ett nytt försök.",
                "Finish signing in in the browser. If the page shows an error or closes, click Sign in again for a new attempt.");
        }
        else
        {
            Report(ack, Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        }
    }

    private async void OnFreeUp(object sender, RoutedEventArgs e)
    {
        if (Engine.IsFreeingUp || !Engine.IsSignedIn)
        {
            return;
        }

        var ack = await Engine.FreeUpSpaceAsync();
        if (!ack.Ok)
        {
            Report(ack, Lang.T("Utrymmet gick inte att frigöra", "The space could not be freed"));
        }
    }

    /// <summary>The failed jobs go back in the queue; the new counts come with the next `status`.</summary>
    private async void OnRetryFailed(object sender, RoutedEventArgs e)
    {
        Report(await Engine.RetryFailedAsync(), Lang.T("Det gick inte att försöka igen", "The retry could not be started"));
    }

    private async void OnOpenLog(object sender, RoutedEventArgs e)
    {
        Report(await Engine.OpenLogAsync(), Lang.T("Loggmappen gick inte att öppna", "The log folder could not be opened"));
    }

    private async void OnOpenProject(object sender, RoutedEventArgs e)
    {
        if (sender is Button { Tag: string path } && !string.IsNullOrEmpty(path))
        {
            Report(await Engine.OpenFolderAsync(path), Lang.T("Mappen gick inte att öppna", "The folder could not be opened"));
        }
    }

    private void OnShowActivity(object sender, RoutedEventArgs e)
    {
        MainWindow.Instance?.NavigateTo(MainWindow.ActivityTag);
    }

    private void OnChooseProjects(object sender, RoutedEventArgs e)
    {
        MainWindow.Instance?.NavigateTo(MainWindow.ProjectsTag);
    }

    private void OnOpenAccounts(object sender, RoutedEventArgs e)
    {
        MainWindow.Instance?.NavigateTo(MainWindow.AccountsTag);
    }

    /// <summary>A refused command goes to the window's message bar with the engine's reason.</summary>
    private static void Report(Ack ack, string what)
    {
        if (ack.Ok)
        {
            return;
        }

        MainWindow.Instance?.ShowMessage(what, ErrorDisplay.FriendlyDetail(ack.Error ?? Lang.T("Okänt fel.", "Unknown error.")), InfoBarSeverity.Error);
    }
}
