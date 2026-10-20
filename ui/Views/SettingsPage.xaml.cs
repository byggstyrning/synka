using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using CommunityToolkit.WinUI.Controls;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Windows.Storage.Pickers;

namespace Synka.Views;

public sealed partial class SettingsPage : Page
{
    /// <summary>How long the spinner waits for the engine's `update` after a check was accepted.</summary>
    private static readonly TimeSpan UpdateAnswerTimeout = TimeSpan.FromSeconds(30);

    /// <summary>How long "Installerar …" is shown when the engine never reports the install.</summary>
    private static readonly TimeSpan InstallAnswerTimeout = TimeSpan.FromSeconds(60);

    private readonly EngineClient _engine = EngineClient.Instance;

    // Five minutes for the browser, bounded token/profile calls, then reconnect margin.
    private static readonly TimeSpan ApsClientIdAnswerTimeout = TimeSpan.FromMinutes(8);
    private ApsClientIdEditState _apsClientIdEdit => _engine.ApsClientIdEdit;
    private bool _apsMirroring;

    /// <summary>
    /// True while the page mirrors the engine's values into the controls. Without it
    /// the Toggled and ValueChanged events would take the stored value for a user
    /// choice and send it straight back to the engine.
    /// </summary>
    private bool _mirroring;

    /// <summary>True between "check for updates" and the engine's answer. The spinner runs meanwhile.</summary>
    private bool _awaitingUpdate;

    /// <summary>True between "install" and the engine's next `update`.</summary>
    private bool _installRequested;

    /// <summary>Counts the checks and installs, so a late timeout does not reset a newer one.</summary>
    private int _updateToken;

    /// <summary>A refusal of the last update command, shown until the engine says something newer.</summary>
    private string? _updateProblem;

    /// <summary>
    /// The StreamBIM server the user picked in the form: a region's subdomain, or <see cref="OtherServerTag"/>
    /// for a custom address. Null until they pick one: the engine's default is used then.
    /// </summary>
    private string? _streamBimRegionChoice;

    /// <summary>The server list's last item: a custom https address, typed in the address box. Not a valid subdomain.</summary>
    private const string OtherServerTag = "#other";

    /// <summary>True while the region list is rebuilt: the ComboBox's selection is then not the user's choice.</summary>
    private bool _populatingRegions;

    /// <summary>The account the form last sent a sign-in for, or whose sign-in failed. Its message is shown, and a success resets the form.</summary>
    private string? _streamBimFormKey;

    /// <summary>A sign-in from the form is on its way to the engine: Logga in waits for the answer.</summary>
    private bool _streamBimSending;

    /// <summary>The account cards above the form, kept so they can be replaced when the accounts change.</summary>
    private readonly List<SettingsCard> _streamBimRows = new();

    /// <summary>What the account cards show last. They are rebuilt only when this changes.</summary>
    private string _streamBimRowsSignature = string.Empty;

    /// <summary>The user has typed in the Interaxo fields since the last save: the engine's values then wait.</summary>
    private bool _interaxoEdited;

    /// <summary>The StreamBIM MFA dialog while it is open. Null otherwise.</summary>
    private ContentDialog? _mfaDialog;

    /// <summary>The StreamBIM account whose code the open MFA dialog asks for. Null when no dialog is open.</summary>
    private string? _mfaAccount;

    /// <summary>The code box of the open MFA dialog.</summary>
    private TextBox? _mfaCode;

    /// <summary>The code has gone to the engine: Verify stays off until the engine's next progress event.</summary>
    private bool _mfaSubmitted;

    /// <summary>The engine ended the MFA wait (signed in, failed or cancelled, or the connection went), so closing the dialog must not cancel again.</summary>
    private bool _mfaEndedByEngine;

    /// <summary>
    /// The code prompts the user closed with Avbryt, per StreamBIM account. A prompt does not come back
    /// while the engine still reports that same prompt (before its `cancelled` arrives).
    /// </summary>
    private readonly Dictionary<string, SignInProgressInfo> _mfaDismissed = new();

    /// <summary>
    /// The failure a provider's card no longer shows: closed by the user, or superseded by a
    /// retry. The same failure does not come back on the next status; a new one (a new message) does.
    /// Keyed by <see cref="FailureKey"/>.
    /// </summary>
    private readonly Dictionary<string, SignInProgressInfo> _dismissedFailures = new();

    /// <summary>True from Flytta… until the engine has answered: the folder buttons wait meanwhile.</summary>
    private bool _choosingFolder;

    private bool _attached;

    public SettingsPage()
    {
        InitializeComponent();

        _mirroring = true;
        ThemeChoice.SelectedIndex = MainWindow.CurrentTheme switch
        {
            ElementTheme.Light => 0,
            ElementTheme.Dark => 1,
            _ => 2,
        };
        LanguageChoice.SelectedIndex = Lang.Current == AppLanguage.English ? 1 : 0;
        _mirroring = false;

        Loaded += OnLoaded;
        Unloaded += OnUnloaded;
    }

    private void OnLoaded(object sender, RoutedEventArgs args)
    {
        if (_attached)
        {
            return;
        }
        _attached = true;

        _engine.PropertyChanged += OnEngineChanged;
        _engine.SignInProgress += OnSignInProgress;
        _engine.StreamBimRegionsChanged += OnStreamBimRegionsChanged;
        _engine.Notified += OnApsClientIdNotified;
        RefreshSettings();
        RefreshAccounts();
        RefreshEnabled();
        ShowUpdate();

        // The cached list shows at once; the engine's list follows when it answers.
        ShowStreamBimRegions(_engine.StreamBimRegions);
        RequestStreamBimRegions();

        // A signed-in provider's card starts folded; one that needs the user starts open.
        AccExpander.IsExpanded = _engine.AccountFor(ProviderNames.Acc) is not { SignedIn: true };
        StreamBimExpander.IsExpanded = !_engine.StreamBimAccounts.Any(account => account.SignedIn);
        InteraxoExpander.IsExpanded = _engine.AccountFor(ProviderNames.Interaxo) is not { SignedIn: true };

        // The engine may be waiting for an MFA code that was asked for while this page was closed.
        ShowMfaDialogIfWanted();
    }

    private void OnUnloaded(object sender, RoutedEventArgs args)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        _engine.PropertyChanged -= OnEngineChanged;
        _engine.SignInProgress -= OnSignInProgress;
        _engine.StreamBimRegionsChanged -= OnStreamBimRegionsChanged;
        _engine.Notified -= OnApsClientIdNotified;
    }

    // ---------------------------------------------------------- the engine's values

    /// <summary>
    /// Each engine message refreshes only its own part of the page. `status` comes up
    /// to twice a second and must not rewrite the settings controls under the user's
    /// hands (a NumberBox being typed into, say).
    /// </summary>
    private void OnEngineChanged(object? sender, PropertyChangedEventArgs args)
    {
        switch (args.PropertyName)
        {
            case nameof(EngineClient.Update):
                OnUpdateReceived();
                break;

            case nameof(EngineClient.Settings):
                RefreshSettings();
                break;

            case nameof(EngineClient.ApsClientIdValidation):
                RefreshApsClientIdSettings(_engine.Settings);
                break;

            case nameof(EngineClient.IsFreeingUp):
                RefreshEnabled();
                break;

            // Status (accounts included) comes up to twice a second: the account cards and
            // the APS editor follow it, and the folder card with the signed-in state. The
            // engine's `signing_in` can arrive after its MFA progress, so the dialog may open here.
            case nameof(EngineClient.Status):
                RefreshAccounts();
                ShowMfaDialogIfWanted();
                RefreshFolder();
                RefreshApsClientIdControls();
                break;

            case nameof(EngineClient.IsConnected):
                // The engine's MFA wait does not outlive the connection: the dialog closes with it.
                if (!_engine.IsConnected)
                {
                    CloseMfaDialogOnDisconnect();
                }
                RefreshAccounts();
                RefreshFolder();
                RefreshEnabled();
                ShowUpdate();
                RequestStreamBimRegions();
                break;

            case nameof(EngineClient.IsSignedIn):
                RefreshFolder();
                RefreshEnabled();
                ShowUpdate();
                break;
        }
    }

    /// <summary>Copies the engine's last settings into the controls.</summary>
    private void RefreshSettings()
    {
        var settings = _engine.Settings;
        RefreshFolder();
        RefreshApsClientIdSettings(settings);

        // No update channel (the default): the engine never checks, so the update controls
        // are hidden and a note says where new versions come from instead.
        var channel = settings?.UpdateChannelConfigured == true;
        UpdateCard.Visibility = channel ? Visibility.Visible : Visibility.Collapsed;
        UpdateInfoCard.Visibility = channel ? Visibility.Collapsed : Visibility.Visible;
        // auto_update is newer than the rest of the settings: no field, no switch.
        AutoUpdateCard.Visibility = channel && settings?.AutoUpdate is not null ? Visibility.Visible : Visibility.Collapsed;

        if (settings is null)
        {
            return;
        }

        _mirroring = true;
        try
        {
            StartAtLoginSwitch.IsOn = settings.StartAtLogin;
            PropagateSwitch.IsOn = settings.PropagateDeletes;
            SetNumber(MaxDownloadsBox, settings.MaxDownloads);
            SetNumber(MaxUploadsBox, settings.MaxUploads);
            SetNumber(PollBox, settings.PollActiveSecs);
            LogLevelChoice.SelectedIndex = settings.LogLevel == "debug" ? 1 : 0;

            if (settings.AutoUpdate is bool autoUpdate)
            {
                AutoUpdateSwitch.IsOn = autoUpdate;
            }

            RefreshInteraxoSettings(settings);
        }
        finally
        {
            _mirroring = false;
        }
    }

    /// <summary>
    /// Copies the engine's Interaxo client settings into their boxes, unless the user has
    /// typed in them since the last save. The redirect URI shows only when it is not the
    /// default: the default is the box's placeholder, and a blank box saves the default.
    /// </summary>
    private void RefreshInteraxoSettings(SettingsInfo settings)
    {
        var defaultRedirect = settings.InteraxoRedirectUriDefault ?? string.Empty;
        if (!_interaxoEdited)
        {
            var effective = settings.InteraxoRedirectUri ?? string.Empty;
            var redirectText = string.Equals(effective, defaultRedirect, StringComparison.Ordinal) ? string.Empty : effective;
            var clientIdText = settings.InteraxoClientId ?? string.Empty;
            if (InteraxoClientIdBox.Text != clientIdText)
            {
                InteraxoClientIdBox.Text = clientIdText;
            }
            if (InteraxoRedirectBox.Text != redirectText)
            {
                InteraxoRedirectBox.Text = redirectText;
            }
        }

        InteraxoRedirectBox.PlaceholderText = defaultRedirect;
        InteraxoSecretBox.PlaceholderText = settings.InteraxoClientSecretSet
            ? Lang.T("Sparad – lämna tomt för att behålla", "Saved – leave blank to keep it")
            : Lang.T("Valfri, om Interaxo utfärdar en hemlighet", "Optional, if Interaxo issues a secret");
        Show(InteraxoClearSecretButton, settings.InteraxoClientSecretSet);
    }

    private static void SetNumber(NumberBox box, int value)
    {
        if (box.Value != value)
        {
            box.Value = value;
        }
    }

    /// <summary>Every control that talks to the engine waits for the connection.</summary>
    private void RefreshEnabled()
    {
        var connected = _engine.IsConnected;
        OpenFolderButton.IsEnabled = connected;
        StartAtLoginSwitch.IsEnabled = connected;
        PropagateSwitch.IsEnabled = connected;
        MaxDownloadsBox.IsEnabled = connected;
        MaxUploadsBox.IsEnabled = connected;
        PollBox.IsEnabled = connected;
        FreeUpButton.IsEnabled = _engine.IsSignedIn && !_engine.IsFreeingUp;
        FreeUpProgress.IsActive = _engine.IsFreeingUp;
        FreeUpProgress.Visibility = _engine.IsFreeingUp ? Visibility.Visible : Visibility.Collapsed;
        OpenLogButton.IsEnabled = connected;
        ExportDiagnosticsButton.IsEnabled = connected;
        LogLevelChoice.IsEnabled = connected;
        AutoUpdateSwitch.IsEnabled = connected;
        RestartEngineButton.IsEnabled = connected;
        RefreshApsClientIdControls();
    }

    // ------------------------------------------------------------- accounts

    /// <summary>
    /// Writes every provider's state into its card and what the user may still do there.
    /// Runs with each status and sign-in message, so it writes only the values that changed.
    /// </summary>
    private void RefreshAccounts()
    {
        RefreshAccAccount();
        RefreshStreamBimAccounts();
        RefreshStreamBimForm();
        RefreshInteraxoAccount();
    }

    private void RefreshAccAccount()
    {
        var connected = _engine.IsConnected;
        var account = connected ? _engine.AccountFor(ProviderNames.Acc) : null;
        var signedIn = account is { SignedIn: true };

        SetDescription(AccExpander,
            !connected ? NoContactText
            : account is null ? Lang.T("Hämtar kontot …", "Fetching the account …")
            : account.SigningIn ? Lang.T("Väntar på webbläsaren…", "Waiting for the browser…")
            : signedIn ? SignedInAs(account.User)
            : Lang.T("Inte inloggad", "Not signed in"));

        // While the browser is open, Logga in stays usable: the engine replaces the pending
        // attempt, which is the retry after a closed tab. Avbryt ends that attempt instead.
        Show(AccSignInButton, account is { SignedIn: false });
        Show(AccCancelSignInButton, account is { SignedIn: false, SigningIn: true });
        AccSignInButton.IsEnabled = connected
            && account is { Configured: true, SignedIn: false }
            && !_apsClientIdEdit.IsWaiting;
        AccCancelSignInButton.IsEnabled = connected;
        Show(AccSignOutButton, signedIn);
        AccSignOutButton.IsEnabled = connected && signedIn;
        ShowFailure(AccError, ProviderNames.Acc);
    }

    /// <summary>
    /// The StreamBIM expander's line, and one card per account the engine reports as signed in or
    /// signing in, above the form. The cards are rebuilt only when what they show changes.
    /// </summary>
    private void RefreshStreamBimAccounts()
    {
        var connected = _engine.IsConnected;
        var accounts = connected ? _engine.StreamBimAccounts : Array.Empty<AccountInfo>();
        var signedIn = accounts.Where(account => account.SignedIn).ToList();
        var signingIn = accounts.Where(account => !account.SignedIn && account.SigningIn).ToList();

        string state;
        if (!connected)
        {
            state = NoContactText;
        }
        else if (_engine.Status is null)
        {
            state = Lang.T("Hämtar kontot …", "Fetching the account …");
        }
        else if (_engine.Status.Accounts is null)
        {
            // An older engine has no StreamBIM at all.
            state = Lang.T("Uppdatera motorn för att logga in här.", "Update the engine to sign in here.");
        }
        else if (signedIn.Count == 1)
        {
            state = SignedInAs(signedIn[0].User) + " · " + ServerHost(signedIn[0]);
        }
        else if (signedIn.Count > 1)
        {
            state = Lang.T($"Inloggad på {signedIn.Count} StreamBIM-servrar", $"Signed in to {signedIn.Count} StreamBIM servers");
        }
        else if (signingIn.Count > 0)
        {
            state = signingIn.Any(WaitsForCode)
                ? Lang.T("Väntar på koden från autentiseringsappen…", "Waiting for the code from your authenticator app…")
                : Lang.T("Loggar in på StreamBIM…", "Signing in to StreamBIM…");
        }
        else
        {
            state = Lang.T("Inte inloggad", "Not signed in");
        }
        SetDescription(StreamBimExpander, state);

        // An account without its key cannot be told apart from the others, so it gets no card (and no buttons).
        var rows = accounts
            .Where(account => (account.SignedIn || account.SigningIn) && !string.IsNullOrEmpty(account.Account))
            .ToList();
        var signature = connected + "\n" + string.Join("\n", rows.Select(account => string.Join("|",
            account.Account, account.Host, account.SignedIn, account.SigningIn, account.User?.Name, account.User?.Email,
            WaitsForCode(account), ProviderNames.StreamBimName(account, _engine.StreamBimRegions))));
        if (signature == _streamBimRowsSignature)
        {
            return;
        }
        _streamBimRowsSignature = signature;

        foreach (var card in _streamBimRows)
        {
            StreamBimExpander.Items.Remove(card);
        }
        _streamBimRows.Clear();
        foreach (var account in rows)
        {
            _streamBimRows.Add(BuildStreamBimRow(account, connected));
        }
        for (var index = 0; index < _streamBimRows.Count; index++)
        {
            StreamBimExpander.Items.Insert(index, _streamBimRows[index]);
        }
    }

    /// <summary>One account's card: its hub-style name, the server's host and its state, with Logga ut (signed in) or Avbryt (signing in).</summary>
    private SettingsCard BuildStreamBimRow(AccountInfo account, bool connected)
    {
        var key = account.Account ?? string.Empty;
        var host = ServerHost(account);
        var signedIn = account.SignedIn;
        var state = signedIn
            ? SignedInAs(account.User)
            : WaitsForCode(account)
                ? Lang.T("Väntar på kod…", "Waiting for code…")
                : Lang.T("Loggar in…", "Signing in…");

        var button = new Button
        {
            Content = signedIn ? Lang.T("Logga ut", "Sign out") : Lang.T("Avbryt", "Cancel"),
            IsEnabled = connected,
        };
        AutomationProperties.SetName(button, signedIn
            ? Lang.T($"Logga ut från StreamBIM, {host}", $"Sign out of StreamBIM, {host}")
            : Lang.T($"Avbryt inloggningen på StreamBIM, {host}", $"Cancel the sign-in to StreamBIM, {host}"));
        button.Click += async (_, _) =>
        {
            if (signedIn)
            {
                await SignOutStreamBimAsync(key);
            }
            else
            {
                await CancelStreamBimSignInAsync(key);
            }
        };

        return new SettingsCard
        {
            Header = ProviderNames.StreamBimName(account, _engine.StreamBimRegions),
            Description = host + " · " + state,
            Content = button,
        };
    }

    /// <summary>
    /// The sign-in form: the server list (a server that is signed in is disabled and marked), the own
    /// address with its warning and inline check, and Logga in. Nothing here is sent by itself.
    /// </summary>
    private void RefreshStreamBimForm()
    {
        var connected = _engine.IsConnected;
        // An older engine reports no accounts and takes no StreamBIM sign-in: the form waits for an update.
        var editable = connected && _engine.Status?.Accounts is not null;
        var signedIn = new HashSet<string>(
            _engine.StreamBimAccounts
                .Where(account => account.SignedIn && !string.IsNullOrEmpty(account.Account))
                .Select(account => account.Account!),
            StringComparer.Ordinal);

        foreach (var item in StreamBimRegionBox.Items.OfType<ComboBoxItem>())
        {
            if (item.Tag is not string tag || tag == OtherServerTag)
            {
                continue;
            }
            var taken = signedIn.Contains(tag);
            var content = taken ? RegionTitle(tag) + " " + Lang.T("(inloggad)", "(signed in)") : RegionTitle(tag);
            if (!Equals(item.Content, content))
            {
                item.Content = content;
            }
            item.IsEnabled = !taken;
        }
        EnsureServerPicked();

        var other = IsOtherServerPicked();
        Show(StreamBimUrlBox, other);
        Show(StreamBimUrlWarning, other);

        var urlReady = false;
        string? urlError = null;
        if (other)
        {
            var typed = StreamBimUrlBox.Text.Trim();
            if (StreamBimAddress.TryParse(typed, out var address, out var problem))
            {
                if (signedIn.Contains(address.Key))
                {
                    urlError = Lang.T(
                        $"Du är redan inloggad på {address.Host} – logga ut först.",
                        $"You are already signed in to {address.Host} – sign out first.");
                }
                else
                {
                    urlReady = true;
                }
            }
            else if (typed.Length > 0)
            {
                urlError = problem;
            }
        }
        if (StreamBimUrlError.Text != (urlError ?? string.Empty))
        {
            StreamBimUrlError.Text = urlError ?? string.Empty;
        }
        Show(StreamBimUrlError, urlError is not null);

        var ready = editable && !_streamBimSending
            && StreamBimEmailBox.Text.Trim().Length > 0
            && StreamBimPasswordBox.Password.Length > 0
            && (other ? urlReady : SelectedRegion() is not null);
        StreamBimRegionBox.IsEnabled = editable;
        StreamBimUrlBox.IsEnabled = editable;
        StreamBimEmailBox.IsEnabled = editable;
        StreamBimPasswordBox.IsEnabled = editable;
        StreamBimSignInButton.IsEnabled = ready;
        ShowFailure(StreamBimError, ProviderNames.StreamBim, _streamBimFormKey);
    }

    /// <summary>The engine waits for this StreamBIM account's code: its latest progress says mfa_required.</summary>
    private bool WaitsForCode(AccountInfo account) =>
        !account.SignedIn && _engine.SignInProgressFor(ProviderNames.StreamBim, account.Account)?.State == "mfa_required";

    /// <summary>The server's host: the region's `&lt;subdomain&gt;.streambim.com` or the custom address's host, or the account key when the engine sends none.</summary>
    private static string ServerHost(AccountInfo account) =>
        string.IsNullOrWhiteSpace(account.Host) ? account.Account ?? string.Empty : account.Host;

    /// <summary>The host of a StreamBIM account by its key, for the words the window shows.</summary>
    private string ServerHostFor(string account) =>
        _engine.StreamBimAccounts.FirstOrDefault(info => info.Account == account) is { } found
            ? ServerHost(found)
            : account;

    /// <summary>A region's title from the engine's list, or its subdomain when the list does not name it.</summary>
    private string RegionTitle(string subdomain)
    {
        var title = _engine.StreamBimRegions?.Regions?
            .FirstOrDefault(region => region is not null && region.Subdomain == subdomain)?.Title;
        return string.IsNullOrWhiteSpace(title) ? subdomain : title.Trim();
    }

    private void RefreshInteraxoAccount()
    {
        var connected = _engine.IsConnected;
        var account = connected ? _engine.AccountFor(ProviderNames.Interaxo) : null;
        var signedIn = account is { SignedIn: true };

        string state;
        if (!connected)
        {
            state = NoContactText;
        }
        else if (_engine.Status is null)
        {
            state = Lang.T("Hämtar kontot …", "Fetching the account …");
        }
        else if (account is null)
        {
            state = Lang.T("Uppdatera motorn för att logga in här.", "Update the engine to sign in here.");
        }
        else if (signedIn)
        {
            state = SignedInAs(account.User);
        }
        else if (account.SigningIn)
        {
            state = Lang.T("Väntar på webbläsaren…", "Waiting for the browser…");
        }
        else if (!account.Configured)
        {
            state = Lang.T("Ange en Client ID nedan först.", "Enter a client ID below first.");
        }
        else
        {
            state = Lang.T("Inte inloggad", "Not signed in");
        }
        SetDescription(InteraxoExpander, state);

        // As ACC: a new Logga in replaces the pending browser sign-in, Avbryt ends it.
        Show(InteraxoSignInButton, account is { SignedIn: false });
        Show(InteraxoCancelSignInButton, account is { SignedIn: false, SigningIn: true });
        InteraxoSignInButton.IsEnabled = account is { SignedIn: false, Configured: true };
        InteraxoCancelSignInButton.IsEnabled = connected;
        Show(InteraxoSignOutButton, signedIn);
        InteraxoSignOutButton.IsEnabled = connected && signedIn;

        // The client settings change only while Interaxo is signed out (docs\IPC.md).
        var editable = account is { SignedIn: false, SigningIn: false };
        InteraxoClientIdBox.IsEnabled = editable;
        InteraxoRedirectBox.IsEnabled = editable;
        InteraxoSecretBox.IsEnabled = editable;
        InteraxoSaveButton.IsEnabled = editable;
        InteraxoClearSecretButton.IsEnabled = editable;
        ShowFailure(InteraxoError, ProviderNames.Interaxo);
    }

    /// <summary>
    /// Shows a provider's failed sign-in in its card, once. A failure the user has closed or
    /// already retried stays hidden until the engine reports a new one. A cancelled sign-in
    /// shows only when the engine says why (the user's own Avbryt says nothing).
    /// </summary>
    private void ShowFailure(InfoBar bar, string provider, string? account = null)
    {
        var progress = _engine.SignInProgressFor(provider, account);
        var reported = progress is { State: "failed" }
            || (progress is { State: "cancelled" } && !string.IsNullOrEmpty(progress.Error));
        var failed = reported
            && !(_dismissedFailures.TryGetValue(FailureKey(provider, account), out var dismissed) && ReferenceEquals(dismissed, progress));

        var message = string.Empty;
        if (failed)
        {
            message = ErrorDisplay.FriendlyDetail(progress!.Error);
            if (message.Length == 0)
            {
                message = Lang.T("Inloggningen misslyckades.", "Sign-in failed.");
            }
        }

        if (bar.Message != message)
        {
            bar.Message = message;
        }
        if (bar.IsOpen != failed)
        {
            bar.IsOpen = failed;
        }
    }

    /// <summary>Remembers the failure on screen as seen: a new sign-in attempt hides the old message.</summary>
    private void DismissFailure(string provider, string? account = null)
    {
        if (_engine.SignInProgressFor(provider, account) is { } progress)
        {
            _dismissedFailures[FailureKey(provider, account)] = progress;
        }
    }

    private static string FailureKey(string provider, string? account) => provider + "/" + (account ?? string.Empty);

    private void OnProviderErrorClosed(InfoBar sender, InfoBarClosedEventArgs args)
    {
        if (sender.Tag is string provider)
        {
            DismissFailure(provider);
        }
    }

    /// <summary>The state of an account in words: "Inloggad som Anna (anna@firma.se)".</summary>
    private static string SignedInAs(UserInfo? user)
    {
        var who = ProviderNames.Who(user);
        return who.Length == 0
            ? Lang.T("Inloggad.", "Signed in.")
            : Lang.T($"Inloggad som {who}", $"Signed in as {who}");
    }

    private static string NoContactText => Lang.T("Ingen kontakt med motorn.", "No contact with the engine.");

    private static void Show(UIElement element, bool visible)
    {
        var wanted = visible ? Visibility.Visible : Visibility.Collapsed;
        if (element.Visibility != wanted)
        {
            element.Visibility = wanted;
        }
    }

    private static void SetDescription(SettingsExpander expander, string text)
    {
        if (!Equals(expander.Description, text))
        {
            expander.Description = text;
        }
    }

    // ----------------------------------------------------------- sign-in actions

    private async void OnAccSignIn(object sender, RoutedEventArgs args)
    {
        DismissFailure(ProviderNames.Acc);
        ReportIfFailed(await _engine.SignInAsync(ProviderNames.Acc),
            Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        RefreshAccounts();
    }

    /// <summary>Ends the browser sign-in that is waiting (the tab was closed, say). The engine reports `cancelled`.</summary>
    private async void OnAccCancelSignIn(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.CancelSignInAsync(ProviderNames.Acc),
            Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        RefreshAccounts();
    }

    private async void OnAccSignOut(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.SignOutAsync(ProviderNames.Acc),
            Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
    }

    /// <summary>The server the user picked, or null when the own address is picked or nothing is.</summary>
    private string? SelectedRegion() =>
        StreamBimRegionBox.SelectedItem is ComboBoxItem { Tag: string tag } && tag.Length > 0 && tag != OtherServerTag
            ? tag
            : null;

    /// <summary>True when the server list's last item, the own address, is picked.</summary>
    private bool IsOtherServerPicked() =>
        StreamBimRegionBox.SelectedItem is ComboBoxItem { Tag: OtherServerTag };

    /// <summary>
    /// Sends a StreamBIM sign-in from the form. The password goes to the engine once: its box is
    /// emptied as the sign-in is sent, and nothing keeps it afterwards. The form follows this account
    /// until the engine reports how the attempt went.
    /// </summary>
    private async void OnStreamBimSignIn(object sender, RoutedEventArgs args)
    {
        string? region = null;
        string? url = null;
        string key;
        if (IsOtherServerPicked())
        {
            if (!StreamBimAddress.TryParse(StreamBimUrlBox.Text, out var address, out _))
            {
                RefreshStreamBimForm();
                return;
            }
            url = address.Url;
            key = address.Key;
        }
        else if (SelectedRegion() is { } chosen)
        {
            region = chosen;
            key = chosen;
        }
        else
        {
            RefreshStreamBimForm();
            return;
        }

        var username = StreamBimEmailBox.Text.Trim();
        var password = StreamBimPasswordBox.Password;
        if (username.Length == 0 || password.Length == 0)
        {
            RefreshStreamBimForm();
            return;
        }
        StreamBimPasswordBox.Password = string.Empty;

        _streamBimFormKey = key;
        DismissFailure(ProviderNames.StreamBim, key);
        _streamBimSending = true;
        RefreshStreamBimForm();
        try
        {
            var ack = await _engine.SignInStreamBimAsync(region, url, username, password);
            ReportIfFailed(ack, Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        }
        finally
        {
            _streamBimSending = false;
            RefreshAccounts();
        }
    }

    /// <summary>Signs out of one StreamBIM account (its card's Logga ut).</summary>
    private async Task SignOutStreamBimAsync(string account)
    {
        ReportIfFailed(await _engine.SignOutAsync(ProviderNames.StreamBim, account),
            Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
    }

    /// <summary>Ends one StreamBIM account's sign-in while it waits (its card's Avbryt). The engine reports `cancelled`.</summary>
    private async Task CancelStreamBimSignInAsync(string account)
    {
        ReportIfFailed(await _engine.CancelSignInAsync(ProviderNames.StreamBim, account),
            Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        RefreshAccounts();
    }

    /// <summary>The server the user picks is remembered, and the form follows it.</summary>
    private void OnStreamBimRegionChanged(object sender, SelectionChangedEventArgs args)
    {
        if (!_populatingRegions && StreamBimRegionBox.SelectedItem is ComboBoxItem { Tag: string tag } && tag.Length > 0)
        {
            _streamBimRegionChoice = tag;
        }
        RefreshStreamBimForm();
    }

    private void OnStreamBimFieldChanged(object sender, TextChangedEventArgs args) => RefreshStreamBimForm();

    private void OnStreamBimPasswordChanged(object sender, RoutedEventArgs args) => RefreshStreamBimForm();

    /// <summary>The user closed the StreamBIM message: it stays closed until the engine reports a new failure.</summary>
    private void OnStreamBimErrorClosed(InfoBar sender, InfoBarClosedEventArgs args) =>
        DismissFailure(ProviderNames.StreamBim, _streamBimFormKey);

    /// <summary>After a sign-in succeeded: the e-mail field is cleared, and the form starts over with the default server.</summary>
    private void ResetStreamBimForm()
    {
        _streamBimFormKey = null;
        _streamBimRegionChoice = null;
        StreamBimEmailBox.Text = string.Empty;
        StreamBimPasswordBox.Password = string.Empty;
        StreamBimUrlBox.Text = string.Empty;
        _populatingRegions = true;
        try
        {
            StreamBimRegionBox.SelectedItem = null;
        }
        finally
        {
            _populatingRegions = false;
        }
        RefreshStreamBimForm();
    }

    /// <summary>
    /// Keeps a server picked in the list. When the selection is gone or cannot be picked (its server is
    /// signed in now), the picks are tried in order: the server the user chose last, the default region,
    /// the first region not signed in, and the own address.
    /// </summary>
    private void EnsureServerPicked()
    {
        if (StreamBimRegionBox.SelectedItem is ComboBoxItem { IsEnabled: true })
        {
            return;
        }

        var enabled = StreamBimRegionBox.Items.OfType<ComboBoxItem>().Where(item => item.IsEnabled).ToList();
        var defaultRegion = _engine.StreamBimRegions?.Regions?
            .FirstOrDefault(region => region is { IsDefault: true })?.Subdomain;
        var pick = enabled.FirstOrDefault(item => item.Tag is string tag && tag == _streamBimRegionChoice)
            ?? enabled.FirstOrDefault(item => item.Tag is string tag && tag == defaultRegion)
            ?? enabled.FirstOrDefault(item => item.Tag is string tag && tag != OtherServerTag)
            ?? enabled.FirstOrDefault();

        _populatingRegions = true;
        try
        {
            StreamBimRegionBox.SelectedItem = pick;
        }
        finally
        {
            _populatingRegions = false;
        }
    }

    private void RequestStreamBimRegions()
    {
        if (_engine.IsConnected)
        {
            _ = RequestStreamBimRegionsAsync();
        }
    }

    private async Task RequestStreamBimRegionsAsync()
    {
        ReportIfFailed(await _engine.RequestStreamBimRegionsAsync(),
            Lang.T("Regionerna gick inte att hämta", "The regions could not be fetched"));
    }

    private void OnStreamBimRegionsChanged(StreamBimRegionsInfo regions) => ShowStreamBimRegions(regions);

    /// <summary>
    /// Fills the server list from the region list, then "Other address". The engine's list comes
    /// with its own default, and the user's pick stays when it can still be picked.
    /// </summary>
    private void ShowStreamBimRegions(StreamBimRegionsInfo? info)
    {
        var regions = (info?.Regions ?? Array.Empty<StreamBimRegion>())
            .Where(region => region is not null && !string.IsNullOrWhiteSpace(region.Subdomain))
            .ToList();

        _populatingRegions = true;
        try
        {
            StreamBimRegionBox.Items.Clear();
            foreach (var region in regions)
            {
                StreamBimRegionBox.Items.Add(new ComboBoxItem { Tag = region.Subdomain });
            }
            // The own address is offered once there is a list to choose from.
            if (regions.Count > 0)
            {
                StreamBimRegionBox.Items.Add(new ComboBoxItem
                {
                    Content = Lang.T("Annan adress…", "Other address…"),
                    Tag = OtherServerTag,
                });
            }
        }
        finally
        {
            _populatingRegions = false;
        }

        RefreshStreamBimForm();
    }

    /// <summary>
    /// The StreamBIM account whose engine waits for a code and whose prompt the user has not closed
    /// with Avbryt; null when there is none. Several accounts can wait: their prompts come one after the other.
    /// </summary>
    private string? StreamBimAccountWaitingForCode()
    {
        if (!_engine.IsConnected)
        {
            return null;
        }

        foreach (var account in _engine.StreamBimAccounts)
        {
            if (account.SignedIn || !account.SigningIn || string.IsNullOrEmpty(account.Account))
            {
                continue;
            }

            var progress = _engine.SignInProgressFor(ProviderNames.StreamBim, account.Account);
            var dismissed = _mfaDismissed.TryGetValue(account.Account, out var closed) && ReferenceEquals(closed, progress);
            if (progress is { State: "mfa_required" } && !dismissed)
            {
                return account.Account;
            }
        }
        return null;
    }

    /// <summary>Opens the code prompt when an account waits for its code and no prompt is open yet.</summary>
    private void ShowMfaDialogIfWanted()
    {
        if (_mfaDialog is null && StreamBimAccountWaitingForCode() is { } account)
        {
            _ = ShowMfaDialogAsync(account);
        }
    }

    /// <summary>
    /// The engine ended an account's code wait (signed in, failed or cancelled): its prompt closes, and
    /// the card shows what the engine said. Another account's prompt stays open.
    /// </summary>
    private void HideMfaDialog(string account)
    {
        if (_mfaDialog is { } dialog && account == _mfaAccount)
        {
            _mfaEndedByEngine = true;
            dialog.Hide();
        }
    }

    /// <summary>The engine's code waits do not outlive the connection: any open prompt closes with it.</summary>
    private void CloseMfaDialogOnDisconnect()
    {
        if (_mfaDialog is { } dialog)
        {
            _mfaEndedByEngine = true;
            dialog.Hide();
        }
    }

    /// <summary>Verify is usable for six digits, and not between the submit and the engine's next progress event.</summary>
    private void UpdateMfaVerify()
    {
        if (_mfaDialog is { } dialog)
        {
            dialog.IsPrimaryButtonEnabled = !_mfaSubmitted && _mfaCode is { } box && box.Text.Length == 6;
        }
    }

    /// <summary>
    /// The StreamBIM code prompt of one account: six digits from its authenticator app, naming its server.
    /// Verify sends the code once and the dialog stays open until the engine answers; Avbryt (or Esc)
    /// cancels that account's sign-in in the engine.
    /// </summary>
    private async Task ShowMfaDialogAsync(string account)
    {
        if (_mfaDialog is not null || XamlRoot is not { } root)
        {
            return;
        }

        var host = ServerHostFor(account);
        _mfaAccount = account;
        _mfaEndedByEngine = false;
        _mfaSubmitted = false;
        var code = new TextBox
        {
            MaxLength = 6,
            PlaceholderText = "000000",
            IsSpellCheckEnabled = false,
            IsTextPredictionEnabled = false,
        };
        AutomationProperties.SetName(code, Lang.T("Verifieringskod", "Verification code"));
        _mfaCode = code;

        var body = new StackPanel { Spacing = 8 };
        body.Children.Add(Line(Lang.T(
            $"Skriv in de sex siffrorna från din autentiseringsapp för StreamBIM på {host}.",
            $"Enter the six digits from your authenticator app for StreamBIM at {host}.")));
        body.Children.Add(code);

        var dialog = new ContentDialog
        {
            XamlRoot = root,
            // The dialog hangs in the popup root and does not inherit the theme choice.
            RequestedTheme = MainWindow.CurrentTheme,
            Title = Lang.T($"Ange koden för {host}", $"Enter the code for {host}"),
            Content = body,
            PrimaryButtonText = Lang.T("Verifiera", "Verify"),
            CloseButtonText = Lang.T("Avbryt", "Cancel"),
            DefaultButton = ContentDialogButton.Primary,
            IsPrimaryButtonEnabled = false,
        };

        // Digits only, six at most: the engine refuses anything else.
        code.TextChanged += (_, _) =>
        {
            var digits = new string(code.Text.Where(character => character is >= '0' and <= '9').Take(6).ToArray());
            if (digits != code.Text)
            {
                code.Text = digits;
                return;
            }
            UpdateMfaVerify();
        };

        // The dialog does not close on Verify: it stays until the engine answers, so one code goes out.
        dialog.PrimaryButtonClick += (sender, args) =>
        {
            args.Cancel = true;
            if (_mfaSubmitted || code.Text.Length != 6)
            {
                return;
            }

            _mfaSubmitted = true;
            UpdateMfaVerify();
            _ = SubmitMfaCodeAsync(account, code.Text);
        };

        _mfaDialog = dialog;
        try
        {
            await dialog.ShowAsync();
        }
        catch (Exception)
        {
            // Another dialog is open, or the window is closing. Nothing can be asked: this account's sign-in is cancelled.
            _mfaDialog = null;
            _mfaCode = null;
            _mfaAccount = null;
            _ = await _engine.CancelSignInAsync(ProviderNames.StreamBim, account);
            MainWindow.Instance?.ShowMessage(
                Lang.T("Kodfönstret kunde inte visas", "The code dialog could not be shown"),
                Lang.T("Logga in på StreamBIM igen för att försöka på nytt.", "Sign in to StreamBIM again to try again."),
                InfoBarSeverity.Warning);
            return;
        }

        _mfaDialog = null;
        _mfaCode = null;
        _mfaAccount = null;
        if (_mfaEndedByEngine)
        {
            // The engine ended the wait (signed in, failed or cancelled) and closed the dialog itself.
            // A prompt for another account that came meanwhile (the dialog was still closing) opens now.
            RefreshAccounts();
            ShowMfaDialogIfWanted();
            return;
        }

        // Closed by the user (Avbryt or Esc): that prompt is not shown again, and the account's sign-in ends.
        if (_engine.SignInProgressFor(ProviderNames.StreamBim, account) is { } prompt)
        {
            _mfaDismissed[account] = prompt;
        }
        ReportIfFailed(await _engine.CancelSignInAsync(ProviderNames.StreamBim, account),
            Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        RefreshAccounts();

        // Another account may have asked for its code meanwhile: its prompt comes now.
        ShowMfaDialogIfWanted();
    }

    /// <summary>
    /// Sends the typed code once. The engine's next progress event decides what happens next: it
    /// closes the dialog (signed in, failed, cancelled) or, after a new code prompt, re-enables Verify.
    /// A refused code brings Verify back at once.
    /// </summary>
    private async Task SubmitMfaCodeAsync(string account, string digits)
    {
        var ack = await _engine.SignInMfaAsync(ProviderNames.StreamBim, account, digits);
        if (!ack.Ok)
        {
            _mfaSubmitted = false;
            UpdateMfaVerify();
            ReportIfFailed(ack, Lang.T("Koden gick inte att skicka", "The code could not be sent"));
        }
        RefreshAccounts();
    }

    /// <summary>
    /// Every sign-in message. A StreamBIM message concerns one account: its code prompt opens or closes,
    /// the form shows a failure of it, and a success resets the form when it was the one sent from there.
    /// </summary>
    private void OnSignInProgress(SignInProgressInfo progress)
    {
        if (progress.Provider == ProviderNames.StreamBim)
        {
            var account = progress.Account;

            // A new progress event answers the code that was sent: Verify is usable again until the next prompt.
            if (account is not null && account == _mfaAccount)
            {
                _mfaSubmitted = false;
                UpdateMfaVerify();
            }

            switch (progress.State)
            {
                case "mfa_required":
                    ShowMfaDialogIfWanted();
                    break;

                case "failed":
                    // The user starts over: the e-mail stays, the password is typed again. The message is shown in the form.
                    StreamBimPasswordBox.Password = string.Empty;
                    _streamBimFormKey = account;
                    if (account is not null)
                    {
                        HideMfaDialog(account);
                    }
                    break;

                case "signed_in":
                    if (account is not null && account == _streamBimFormKey)
                    {
                        ResetStreamBimForm();
                    }
                    if (account is not null)
                    {
                        HideMfaDialog(account);
                    }
                    break;

                case "cancelled":
                    if (account is not null)
                    {
                        HideMfaDialog(account);
                    }
                    break;
            }
        }

        RefreshAccounts();
    }

    // ---------------------------------------------------------- Interaxo settings

    private void OnInteraxoFieldChanged(object sender, TextChangedEventArgs args)
    {
        if (!_mirroring)
        {
            _interaxoEdited = true;
        }
    }

    /// <summary>
    /// Sends the Interaxo client id and redirect URI, and the secret when one is typed. The
    /// secret box is emptied only once the engine has accepted the secret; a refused save keeps
    /// what was typed, so the user can correct it and save again.
    /// </summary>
    private async void OnSaveInteraxo(object sender, RoutedEventArgs args)
    {
        var account = _engine.IsConnected ? _engine.AccountFor(ProviderNames.Interaxo) : null;
        if (account is not { SignedIn: false, SigningIn: false })
        {
            return;
        }

        var clientId = InteraxoClientIdBox.Text.Trim();
        var redirect = InteraxoRedirectBox.Text.Trim();
        var secret = InteraxoSecretBox.Password;

        var ack = await _engine.SetSettingAsync("interaxo_client_id", clientId);
        if (ack.Ok)
        {
            ack = await _engine.SetSettingAsync("interaxo_redirect_uri", redirect);
        }
        if (ack.Ok && secret.Length > 0)
        {
            ack = await _engine.SetSettingAsync("interaxo_client_secret", secret);
            if (ack.Ok && InteraxoSecretBox.Password == secret)
            {
                // Taken by the engine: the box does not keep it. (Text typed meanwhile stays.)
                InteraxoSecretBox.Password = string.Empty;
            }
        }

        if (ack.Ok)
        {
            // The engine's settings event brings the saved values back into the boxes.
            _interaxoEdited = false;
        }
        else
        {
            ReportIfFailed(ack, Lang.T("Interaxo-inställningen gick inte igenom", "The Interaxo setting did not go through"));
        }
    }

    /// <summary>Removes the saved client secret. The engine refuses it while Interaxo is signed in.</summary>
    private async void OnClearInteraxoSecret(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.SetSettingAsync("interaxo_client_secret", string.Empty),
            Lang.T("Hemligheten gick inte att ta bort", "The secret could not be removed"));
    }

    private async void OnInteraxoSignIn(object sender, RoutedEventArgs args)
    {
        DismissFailure(ProviderNames.Interaxo);
        ReportIfFailed(await _engine.SignInAsync(ProviderNames.Interaxo),
            Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        RefreshAccounts();
    }

    /// <summary>Ends the browser sign-in that is waiting. The engine reports `cancelled`.</summary>
    private async void OnInteraxoCancelSignIn(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.CancelSignInAsync(ProviderNames.Interaxo),
            Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        RefreshAccounts();
    }

    private async void OnInteraxoSignOut(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.SignOutAsync(ProviderNames.Interaxo),
            Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
    }

    // ----------------------------------------------------------- folder

    private async void OnOpenFolder(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.OpenFolderAsync(), Lang.T("Mappen gick inte att öppna", "The folder could not be opened"));
    }

    /// <summary>
    /// The ack only says the engine has taken the job. How it went comes as a
    /// notify from the engine (docs\IPC.md, free_up_space), so nothing is claimed here.
    /// </summary>
    private async void OnFreeUpSpace(object sender, RoutedEventArgs args)
    {
        if (_engine.IsFreeingUp || !_engine.IsSignedIn)
        {
            return;
        }

        ReportIfFailed(await _engine.FreeUpSpaceAsync(),
            Lang.T("Utrymmet gick inte att frigöra", "The space could not be freed"));
    }


    // ------------------------------------------------------- optional APS app

    private void RefreshApsClientIdSettings(SettingsInfo? settings)
    {
        var supported = settings?.ApsClientIdOverrideSupported == true;
        ApsClientIdExpander.Visibility = supported ? Visibility.Visible : Visibility.Collapsed;
        if (supported)
        {
            _apsClientIdEdit.ObserveSettings(settings!.ApsClientId);
            if (ApsClientIdBox.Text != _apsClientIdEdit.Draft)
            {
                _apsMirroring = true;
                try { ApsClientIdBox.Text = _apsClientIdEdit.Draft; }
                finally { _apsMirroring = false; }
            }
        }
        else
        {
            _apsClientIdEdit.FailIfPending(_apsClientIdEdit.RequestVersion, null);
        }
        RefreshApsClientIdControls();
    }

    private void RefreshApsClientIdControls()
    {
        var connected = _engine.IsConnected;
        var supported = _engine.Settings?.ApsClientIdOverrideSupported == true;
        var signedOut = _engine.Status is { SignedIn: false };
        var available = connected && supported && signedOut && !_engine.IsSignedIn
            && !_engine.IsFreeingUp && !_engine.IsMovingSyncRoot && !_apsClientIdEdit.IsWaiting;
        ApsClientIdBox.IsEnabled = available;
        ApsClientIdSaveButton.IsEnabled = available && _apsClientIdEdit.CanSave
            && _engine.Settings?.ApsClientIdValidationSupported == true;
        ApsClientIdDefaultButton.IsEnabled = available && _apsClientIdEdit.CanUseDefault;
        ApsClientIdCancelButton.Visibility = _apsClientIdEdit.IsTesting ? Visibility.Visible : Visibility.Collapsed;
        ApsClientIdCancelButton.IsEnabled = connected && _apsClientIdEdit.IsTesting;
        ApsClientIdProgress.IsActive = _apsClientIdEdit.IsWaiting;
        // ACC sign-in waits for an APS test or save in progress: the account cards follow it here.
        RefreshAccounts();

        ApsClientIdStatus.Text = !connected
            ? Lang.T("Ingen kontakt med motorn. Ditt utkast behålls.", "No contact with the engine. Your draft is kept.")
            : !signedOut || _engine.IsSignedIn
                ? Lang.T("Logga ut innan du byter APS-app.", "Sign out before changing the APS app.")
                : _apsClientIdEdit.IsWaiting
                    ? _apsClientIdEdit.IsTesting
                        ? Lang.T("Slutför testinloggningen i webbläsaren. Inställningen sparas bara om PKCE-testet lyckas …", "Complete the test sign-in in your browser. The setting is saved only if the PKCE test succeeds …")
                        : Lang.T("Väntar på att motorn bekräftar ändringen …", "Waiting for the engine to confirm the change …")
                    : _apsClientIdEdit.Problem
                        ?? (_apsClientIdEdit.Saved
                            ? Lang.T("Inställningen är sparad. Logga in i Synka för att börja synka.", "The setting is saved. Sign in to Synka to start syncing.")
                            : _engine.Settings?.ApsClientIdValidationSupported != true
                            ? Lang.T("Uppdatera motorn för att testa och spara en egen APS-app.", "Update the engine to test and save a custom APS app.")
                            : _apsClientIdEdit.HasChanges
                            ? Lang.T("Ändringen är inte sparad.", "The change has not been saved.")
                            : _apsClientIdEdit.Confirmed.Length == 0
                                ? Lang.T("Synkas standardapp används.", "The Synka default app is in use.")
                                : Lang.T("En egen APS-app används.", "A custom APS app is in use."));
    }

    private void OnApsClientIdChanged(object sender, TextChangedEventArgs args)
    {
        if (!_attached || _apsMirroring) return;
        _apsClientIdEdit.Edit(ApsClientIdBox.Text);
        RefreshApsClientIdControls();
    }

    private async void OnSaveApsClientId(object sender, RoutedEventArgs args) =>
        await SaveApsClientIdAsync(ApsClientIdBox.Text);

    private async void OnUseDefaultApsClientId(object sender, RoutedEventArgs args) =>
        await SaveApsClientIdAsync(string.Empty);

    private async Task SaveApsClientIdAsync(string value)
    {
        if (_apsClientIdEdit.IsWaiting || !_engine.IsConnected || _engine.IsSignedIn
            || _engine.Status is not { SignedIn: false }
            || _engine.Settings?.ApsClientIdOverrideSupported != true
            || _engine.IsFreeingUp || _engine.IsMovingSyncRoot) return;

        var normalized = ApsClientIdEditState.Normalize(value);
        var validate = normalized.Length > 0;
        if (validate && _engine.Settings?.ApsClientIdValidationSupported != true) return;
        var request = _apsClientIdEdit.Begin(normalized, validate);
        var validationId = _apsClientIdEdit.RequestId;
        RefreshApsClientIdControls();
        var ack = validate
            ? await _engine.TestApsClientIdAsync(normalized, validationId!)
            : await _engine.SetSettingAsync("aps_client_id", normalized);
        // A settings event may confirm the write before the enqueue ACK arrives.
        if (!_apsClientIdEdit.IsPending(request)) return;
        if (_apsClientIdEdit.ObserveAck(request, ack))
        {
            RefreshApsClientIdControls();
            ReportIfFailed(ack, Lang.T("APS-inställningen gick inte igenom", "The APS setting did not go through"));
            return;
        }

        // A successful ACK only means enqueued; validation has its own correlated result.
        // A changed settings snapshot also recovers a completion lost during restart.
        await Task.Delay(validate ? ApsClientIdAnswerTimeout : TimeSpan.FromSeconds(30));
        if (_apsClientIdEdit.IsPending(request) && validationId is not null)
            await _engine.CancelApsClientIdTestAsync(validationId);
        if (_apsClientIdEdit.FailIfPending(request, Lang.T(
                "Motorn har inte bekräftat ändringen. Ditt utkast behålls; kontrollera anslutningen och försök igen.",
                "The engine has not confirmed the change. Your draft is kept; check the connection and try again.")))
        {
            RefreshApsClientIdControls();
        }
    }

    private void OnApsClientIdNotified(object? sender, NotifyInfo notify)
    {
        if (!_apsClientIdEdit.IsWaiting
            || _apsClientIdEdit.IsTesting
            || notify.Severity is not ("warning" or "error")
            || notify.Body?.Contains("APS", StringComparison.OrdinalIgnoreCase) != true) return;

        // MainWindow displays the actual refusal. Release this editor while preserving
        // the draft so an asynchronous validation/save failure is easy to correct.
        if (_apsClientIdEdit.FailIfPending(_apsClientIdEdit.RequestVersion, Lang.T(
                "Ändringen bekräftades inte. Se meddelandet från motorn.",
                "The change was not confirmed. See the message from the engine.")))
        {
            RefreshApsClientIdControls();
        }
    }

    private async void OnCancelApsClientIdTest(object sender, RoutedEventArgs args)
    {
        if (_apsClientIdEdit.RequestId is not { } requestId || !_engine.IsConnected) return;
        var ack = await _engine.CancelApsClientIdTestAsync(requestId);
        ReportIfFailed(ack, Lang.T("Testet kunde inte avbrytas", "The test could not be cancelled"));
        // Only the correlated cancelled/saved event decides the outcome of a race.
        RefreshApsClientIdControls();
    }

    // ------------------------------------------------------- moving the folder

    /// <summary>The engine's own choice when none is made: %USERPROFILE%\Synka.</summary>
    private static string DefaultSyncRoot =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Synka");

    /// <summary>The folder the engine syncs to. From `settings`, or `status` until settings have come.</summary>
    private string CurrentSyncRoot => _engine.Settings?.SyncRoot ?? _engine.Status?.SyncRoot ?? string.Empty;

    /// <summary>
    /// The sync folder card: the path, and Återställ standardplats only while the folder
    /// is somewhere else. Moving waits for the connection and for a move already under
    /// way; rebuilding needs the account too. Runs on every `status`, so it changes only what it must.
    /// </summary>
    private void RefreshFolder()
    {
        var root = CurrentSyncRoot;
        if (FolderText.Text != root)
        {
            FolderText.Text = root;
        }

        DefaultFolderButton.Visibility = root.Length > 0 && !SamePath(root, DefaultSyncRoot)
            ? Visibility.Visible
            : Visibility.Collapsed;

        var free = _engine.IsConnected && !_engine.IsMovingSyncRoot && !_choosingFolder;
        MoveFolderButton.IsEnabled = free;
        DefaultFolderButton.IsEnabled = free;
        RebuildAllButton.IsEnabled = free && _engine.IsSignedIn;
    }

    private void OnMoveFolder(object sender, RoutedEventArgs args)
    {
        _ = MoveFolderAsync(pick: true);
    }

    private void OnDefaultFolder(object sender, RoutedEventArgs args)
    {
        _ = MoveFolderAsync(pick: false);
    }

    /// <summary>
    /// A move from the choice to the engine's answer: the picked place (or the default),
    /// a question, then move_sync_root. The buttons wait meanwhile, so a second click
    /// cannot open a second picker or dialog.
    /// </summary>
    private async Task MoveFolderAsync(bool pick)
    {
        if (_choosingFolder)
        {
            return;
        }

        _choosingFolder = true;
        RefreshFolder();
        try
        {
            var target = pick ? await PickTargetAsync() : DefaultSyncRoot;
            if (target is not null)
            {
                await ConfirmAndMoveAsync(target);
            }
        }
        finally
        {
            _choosingFolder = false;
            RefreshFolder();
        }
    }

    /// <summary>
    /// The folder picker; the new sync root is Synka inside the picked folder. WinUI 3
    /// pickers need the window's handle. Null when the user cancelled, or picked
    /// something that is not a folder on a disk (a library, say).
    /// </summary>
    private async Task<string?> PickTargetAsync()
    {
        Windows.Storage.StorageFolder? folder;
        try
        {
            var picker = new FolderPicker
            {
                SuggestedStartLocation = PickerLocationId.ComputerFolder,
                ViewMode = PickerViewMode.List,
            };
            picker.FileTypeFilter.Add("*");
            WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(App.Window));
            folder = await picker.PickSingleFolderAsync();
        }
        catch (Exception exception)
        {
            DiagnosticLog.Write("mappväljaren", exception);
            MainWindow.Instance?.ShowMessage(
                Lang.T("Mappväljaren gick inte att öppna", "The folder picker could not be opened"),
                exception.Message,
                InfoBarSeverity.Error);
            return null;
        }

        if (folder is null)
        {
            return null;
        }

        if (string.IsNullOrEmpty(folder.Path))
        {
            MainWindow.Instance?.ShowMessage(
                Lang.T("Synkmappen gick inte att flytta", "The sync folder could not be moved"),
                Lang.T("Välj en mapp på en lokal disk.", "Choose a folder on a local disk."),
                InfoBarSeverity.Warning);
            return null;
        }

        return TargetIn(folder.Path);
    }

    /// <summary>
    /// The new sync root for a picked folder: Synka inside it. A picked folder that is
    /// itself an empty Synka folder is taken as it is, and so is the sync folder itself
    /// (which is then already there).
    /// </summary>
    private string TargetIn(string picked)
    {
        var name = Path.GetFileName(picked.TrimEnd('\\', '/'));
        var isEmptySynka = string.Equals(name, "Synka", StringComparison.OrdinalIgnoreCase) && IsEmptyFolder(picked);
        return isEmptySynka || SamePath(picked, CurrentSyncRoot) ? picked : Path.Combine(picked, "Synka");
    }

    /// <summary>
    /// Asks first, then sends move_sync_root. The engine checks the place (a new or empty
    /// folder on a local NTFS disk, outside OneDrive and the like) and a refusal shows its
    /// reason. Accepted, the engine restarts itself and the window waits for it.
    /// </summary>
    private async Task ConfirmAndMoveAsync(string target)
    {
        if (SamePath(target, CurrentSyncRoot))
        {
            MainWindow.Instance?.ShowMessage(
                string.Empty,
                Lang.T($"Synkmappen ligger redan i {target}.", $"The sync folder is already in {target}."),
                InfoBarSeverity.Informational);
            return;
        }

        var body = new StackPanel { Spacing = 8 };
        body.Children.Add(new TextBlock
        {
            Text = target,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            TextWrapping = TextWrapping.Wrap,
            IsTextSelectionEnabled = true,
        });
        body.Children.Add(Line(Lang.T(
            "Alla projekt byggs om där från molnet. Filerna laddas ner igen när du öppnar dem.",
            "Every project is rebuilt there from the cloud. The files are downloaded again when you open them.")));
        body.Children.Add(Line(Lang.T(
            "Ändringar som inte hunnit synkas sparas först i mappen \"Synka - återställda filer\".",
            "Changes that have not been synced yet are first saved in the folder \"Synka - återställda filer\".")));

        var pending = _engine.Status?.PendingJobs ?? 0;
        if (pending > 0)
        {
            var count = Fmt.Count(pending);
            body.Children.Add(Line(pending == 1
                ? Lang.T("1 ändring väntar fortfarande på att laddas upp.", "1 change is still waiting to be uploaded.")
                : Lang.T($"{count} ändringar väntar fortfarande på att laddas upp.", $"{count} changes are still waiting to be uploaded.")));
        }

        body.Children.Add(Line(Lang.T(
            "Stäng filer som är öppna i andra program innan du fortsätter.",
            "Close files that are open in other programs before you continue.")));

        if (!await ConfirmAsync(
                Lang.T("Flytta synkmappen?", "Move the sync folder?"),
                body,
                Lang.T("Flytta och bygg om", "Move and rebuild")))
        {
            return;
        }

        var ack = await _engine.MoveSyncRootAsync(target);
        if (!ack.Ok)
        {
            ReportIfFailed(ack, Lang.T("Synkmappen gick inte att flytta", "The sync folder could not be moved"));
            return;
        }

        MainWindow.Instance?.ShowMessage(
            string.Empty,
            Lang.T(
                $"Synkmappen flyttas. {AppInfo.Name} startar om och bygger om projekten.",
                $"The sync folder is being moved. {AppInfo.Name} restarts and rebuilds the projects."),
            InfoBarSeverity.Informational);
    }

    private static bool IsEmptyFolder(string path)
    {
        try
        {
            return !Directory.EnumerateFileSystemEntries(path).Any();
        }
        catch (Exception)
        {
            return false;
        }
    }

    /// <summary>Two paths name the same folder: case and a trailing backslash do not count, as in Windows.</summary>
    private static bool SamePath(string a, string b)
    {
        try
        {
            return string.Equals(
                Path.GetFullPath(a).TrimEnd('\\', '/'),
                Path.GetFullPath(b).TrimEnd('\\', '/'),
                StringComparison.OrdinalIgnoreCase);
        }
        catch (Exception)
        {
            // An empty or malformed path names no folder.
            return false;
        }
    }

    // --------------------------------------------------------- sync settings

    private void OnStartAtLoginToggled(object sender, RoutedEventArgs args)
    {
        if (!_mirroring)
        {
            SetSetting("start_at_login", StartAtLoginSwitch.IsOn);
        }
    }

    private void OnPropagateToggled(object sender, RoutedEventArgs args)
    {
        if (!_mirroring)
        {
            SetSetting("propagate_deletes", PropagateSwitch.IsOn);
        }
    }

    private void OnAutoUpdateToggled(object sender, RoutedEventArgs args)
    {
        if (!_mirroring)
        {
            SetSetting("auto_update", AutoUpdateSwitch.IsOn);
        }
    }

    private void OnMaxDownloadsChanged(NumberBox sender, NumberBoxValueChangedEventArgs args)
    {
        SendNumber(sender, args, "max_downloads", needsRestart: true);
    }

    private void OnMaxUploadsChanged(NumberBox sender, NumberBoxValueChangedEventArgs args)
    {
        SendNumber(sender, args, "max_uploads", needsRestart: true);
    }

    private void OnPollChanged(NumberBox sender, NumberBoxValueChangedEventArgs args)
    {
        SendNumber(sender, args, "poll_active_secs", needsRestart: false);
    }

    /// <summary>
    /// Sends a NumberBox value as a whole number (the engine refuses anything else).
    /// A decimal is rounded in the box first, which comes back here as a whole number.
    /// An emptied box goes back to the engine's value.
    /// </summary>
    private void SendNumber(NumberBox box, NumberBoxValueChangedEventArgs args, string key, bool needsRestart)
    {
        if (_mirroring)
        {
            return;
        }

        if (double.IsNaN(args.NewValue))
        {
            RefreshSettings();
            return;
        }

        var whole = Math.Round(args.NewValue);
        if (whole != args.NewValue)
        {
            box.Value = whole;
            return;
        }

        SetSetting(key, (int)whole, needsRestart);
    }

    private void OnLogLevelChanged(object sender, SelectionChangedEventArgs args)
    {
        if (!_mirroring && LogLevelChoice.SelectedItem is ComboBoxItem item && item.Tag is string level)
        {
            SetSetting("log_level", level, needsRestart: true);
        }
    }

    /// <summary>
    /// Sends one setting. If the engine refuses it, the page takes the engine's
    /// value back, so the controls never show a choice that did not happen. A
    /// setting that waits for a restart says so, with the restart one click away.
    /// </summary>
    private async void SetSetting(string key, object value, bool needsRestart = false)
    {
        var ack = await _engine.SetSettingAsync(key, value);
        if (!ack.Ok)
        {
            ReportIfFailed(ack, Lang.T("Inställningen gick inte igenom", "The setting did not go through"));
            RefreshSettings();
            return;
        }

        if (needsRestart)
        {
            MainWindow.Instance?.ShowMessage(
                Lang.T("Gäller efter omstart", "Takes effect after a restart"),
                Lang.T(
                    "Ändringen är sparad och gäller när motorn har startats om.",
                    "The change is saved and takes effect once the engine has restarted."),
                InfoBarSeverity.Informational,
                Lang.T("Starta om motorn", "Restart the engine"),
                () => _ = RestartEngineAsync());
        }
    }

    private void OnRestartEngine(object sender, RoutedEventArgs args)
    {
        _ = RestartEngineAsync();
    }

    private static async Task RestartEngineAsync()
    {
        var ack = await EngineClient.Instance.RestartEngineAsync();
        ReportIfFailed(ack, Lang.T("Motorn gick inte att starta om", "The engine could not be restarted"));
    }

    private void OnLanguageChanged(object sender, SelectionChangedEventArgs args)
    {
        if (_mirroring || LanguageChoice.SelectedItem is not ComboBoxItem item)
        {
            return;
        }

        if (Enum.TryParse(item.Tag as string, out AppLanguage language))
        {
            Lang.Change(language);
        }
    }

    private void OnThemeChanged(object sender, SelectionChangedEventArgs args)
    {
        if (_mirroring || ThemeChoice.SelectedItem is not ComboBoxItem item)
        {
            return;
        }

        if (Enum.TryParse(item.Tag as string, out ElementTheme theme))
        {
            MainWindow.ChangeTheme(theme);
        }
    }

    private async void OnOpenLog(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.OpenLogAsync(), Lang.T("Loggmappen gick inte att öppna", "The log folder could not be opened"));
    }

    /// <summary>The engine writes the zip and reports where it went in a notify; the ack only says it has started.</summary>
    private async void OnExportDiagnostics(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.ExportDiagnosticsAsync(), Lang.T("Diagnostiken gick inte att exportera", "The diagnostics could not be exported"));
    }

    /// <summary>
    /// Every project rebuilt from the cloud where it is, once the user has said yes. Each
    /// project reports in its own notify, so nothing more is shown here on success.
    /// </summary>
    private async void OnRebuildAll(object sender, RoutedEventArgs args)
    {
        var body = new StackPanel { Spacing = 8 };
        body.Children.Add(Line(Lang.T(
            "Varje projekt byggs om från molnet där det ligger. Filerna laddas ner igen när du öppnar dem. Ändringar som inte hunnit synkas sparas först i \"Synka - återställda filer\".",
            "Every project is rebuilt from the cloud where it is. The files are downloaded again when you open them. Changes that have not been synced yet are first saved in \"Synka - återställda filer\".")));

        if (!await ConfirmAsync(
                Lang.T("Bygga om alla projekt?", "Rebuild all projects?"),
                body,
                Lang.T("Bygg om", "Rebuild")))
        {
            return;
        }

        ReportIfFailed(await _engine.ResetAllAsync(), Lang.T("Projekten gick inte att bygga om", "The projects could not be rebuilt"));
    }

    // ----------------------------------------------------------------- updates

    /// <summary>
    /// First press: ask the engine to check. Once an update is known the button says
    /// "Installera nu" and asks the engine to install it. The two steps are
    /// deliberate, so that an install is never one accidental click.
    /// </summary>
    private async void OnUpdateButton(object sender, RoutedEventArgs args)
    {
        var token = ++_updateToken;
        _updateProblem = null;

        if (_engine.Update is { Available: true })
        {
            _installRequested = true;
            ShowUpdate();

            var install = await _engine.InstallUpdateAsync();
            if (!install.Ok)
            {
                _installRequested = false;
                _updateProblem = install.Error ?? Lang.T("Uppdateringen gick inte att starta.", "The update could not be started.");
                ShowUpdate();
                return;
            }

            // Accepted. The engine reports the install as `update` (installing), and
            // the package update closes this window. If neither happens, say so.
            await Task.Delay(InstallAnswerTimeout);
            if (_installRequested && token == _updateToken)
            {
                _installRequested = false;
                ShowUpdate();
            }
            return;
        }

        _awaitingUpdate = true;
        ShowUpdate();

        var check = await _engine.CheckUpdatesAsync();
        if (!check.Ok)
        {
            _awaitingUpdate = false;
            _updateProblem = check.Error ?? Lang.T("Gick inte att söka efter uppdatering.", "The update check could not be started.");
            ShowUpdate();
            return;
        }

        // The answer arrives as the `update` event. If the engine never sends one,
        // the spinner must not run forever.
        await Task.Delay(UpdateAnswerTimeout);
        if (_awaitingUpdate && token == _updateToken)
        {
            _awaitingUpdate = false;
            _updateProblem = Lang.T("Inget svar från motorn.", "No answer from the engine.");
            ShowUpdate();
        }
    }

    private void OnUpdateReceived()
    {
        _awaitingUpdate = false;
        _installRequested = false;
        _updateProblem = null;
        ShowUpdate();
    }

    /// <summary>Writes what the engine last said about updates (and what the page is waiting for) into the card.</summary>
    private void ShowUpdate()
    {
        var update = _engine.Update;
        var installing = update?.Installing == true;
        var busy = _awaitingUpdate || _installRequested || installing;

        // Only IsActive: an idle ring draws nothing but keeps its place, so the button does not move.
        UpdateSpinner.IsActive = busy;
        UpdateButton.IsEnabled = _engine.IsConnected && !busy;
        UpdateButtonLabel.Text = update is { Available: true }
            ? Lang.T("Installera nu", "Install now")
            : update is null
                ? Lang.T("Sök efter uppdatering", "Check for updates")
                : Lang.T("Sök igen", "Check again");

        if (installing)
        {
            UpdateStatus.Text = Lang.T(
                $"Uppdaterar … {AppInfo.Name} startar om när det är klart.",
                $"Updating … {AppInfo.Name} restarts when it is done.");
            return;
        }

        if (_installRequested)
        {
            UpdateStatus.Text = Lang.T("Installerar …", "Installing …");
            return;
        }

        if (_awaitingUpdate)
        {
            UpdateStatus.Text = Lang.T("söker …", "checking …");
            return;
        }

        if (_updateProblem is not null)
        {
            UpdateStatus.Text = _updateProblem;
            return;
        }

        if (update is null)
        {
            UpdateStatus.Text = Lang.T($"du har {EngineClient.AppVersion}", $"you have {EngineClient.AppVersion}");
            return;
        }

        if (update.Available)
        {
            var version = update.Version ?? Lang.T("En ny version", "A new version");
            UpdateStatus.Text = update.Error is { } installError
                ? installError
                : Lang.T(
                    $"{version} finns — du har {EngineClient.AppVersion}",
                    $"{version} is out — you have {EngineClient.AppVersion}");
            return;
        }

        UpdateStatus.Text = update.Error ?? Lang.T(
            $"du har den senaste ({EngineClient.AppVersion})",
            $"you have the latest ({EngineClient.AppVersion})");
    }

    // ------------------------------------------------------------------- misc

    /// <summary>
    /// A question before something that rebuilds projects. Enter answers no. False as
    /// well when the dialog could not open: then nothing was asked and nothing is done.
    /// </summary>
    private async Task<bool> ConfirmAsync(string title, UIElement body, string primary)
    {
        if (XamlRoot is not { } root)
        {
            return false;
        }

        var dialog = new ContentDialog
        {
            XamlRoot = root,
            // The dialog hangs in the popup root and does not inherit the theme choice.
            RequestedTheme = MainWindow.CurrentTheme,
            Title = title,
            Content = body,
            PrimaryButtonText = primary,
            CloseButtonText = Lang.T("Avbryt", "Cancel"),
            // Enter must not rebuild anything by accident.
            DefaultButton = ContentDialogButton.Close,
        };

        try
        {
            return await dialog.ShowAsync() == ContentDialogResult.Primary;
        }
        catch (Exception)
        {
            // Another dialog is open, or the window is closing.
            return false;
        }
    }

    /// <summary>One paragraph of a dialog.</summary>
    private static TextBlock Line(string text) => new() { Text = text, TextWrapping = TextWrapping.Wrap };

    /// <summary>Shows a refused command in the notification bar, with the engine's reason.</summary>
    private static void ReportIfFailed(Ack ack, string what)
    {
        if (ack.Ok)
        {
            return;
        }

        MainWindow.Instance?.ShowMessage(
            what,
            ack.Error ?? Lang.T("Okänt fel.", "Unknown error."),
            InfoBarSeverity.Error);
    }
}
