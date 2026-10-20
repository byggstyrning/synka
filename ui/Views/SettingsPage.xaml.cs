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

    /// <summary>The StreamBIM region the user picked (its subdomain). Null until they pick one: the engine's default is used then.</summary>
    private string? _streamBimRegionChoice;

    /// <summary>True while the region list is rebuilt: the ComboBox's selection is then not the user's choice.</summary>
    private bool _populatingRegions;

    /// <summary>The user has typed in the Interaxo fields since the last save: the engine's values then wait.</summary>
    private bool _interaxoEdited;

    /// <summary>The StreamBIM MFA dialog while it is open. Null otherwise.</summary>
    private ContentDialog? _mfaDialog;

    /// <summary>The engine ended the MFA wait (signed in, failed or cancelled), so closing the dialog must not cancel again.</summary>
    private bool _mfaEndedByEngine;

    /// <summary>
    /// The failure a provider's card no longer shows: closed by the user, or superseded by a
    /// retry. The same failure does not come back on the next status; a new one (a new message) does.
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
        StreamBimExpander.IsExpanded = _engine.AccountFor(ProviderNames.StreamBim) is not { SignedIn: true };
        InteraxoExpander.IsExpanded = _engine.AccountFor(ProviderNames.Interaxo) is not { SignedIn: true };

        // The engine may be waiting for an MFA code that was asked for while this page was closed.
        if (_engine.SignInProgressFor(ProviderNames.StreamBim) is { State: "mfa_required" })
        {
            _ = ShowMfaDialogAsync();
        }
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
            // the APS editor follow it, and the folder card with the signed-in state.
            case nameof(EngineClient.Status):
                RefreshFolder();
                RefreshApsClientIdControls();
                break;

            case nameof(EngineClient.IsConnected):
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
        RefreshStreamBimAccount();
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

        Show(AccSignInButton, account is { SignedIn: false });
        Show(AccSignOutButton, signedIn);
        AccSignInButton.IsEnabled = connected
            && account is { Configured: true, SignedIn: false, SigningIn: false }
            && !_apsClientIdEdit.IsWaiting;
        AccSignOutButton.IsEnabled = connected && signedIn;
        ShowFailure(AccError, ProviderNames.Acc);
    }

    private void RefreshStreamBimAccount()
    {
        var connected = _engine.IsConnected;
        var account = connected ? _engine.AccountFor(ProviderNames.StreamBim) : null;
        var signedIn = account is { SignedIn: true };
        var editable = account is { SignedIn: false, SigningIn: false };

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
            // An older engine has no StreamBIM at all.
            state = Lang.T("Uppdatera motorn för att logga in här.", "Update the engine to sign in here.");
        }
        else if (signedIn)
        {
            var host = string.IsNullOrWhiteSpace(account.Host) ? string.Empty : " · " + account.Host;
            state = SignedInAs(account.User) + host;
        }
        else if (account.SigningIn)
        {
            state = _engine.SignInProgressFor(ProviderNames.StreamBim)?.State == "mfa_required"
                ? Lang.T("Väntar på koden från autentiseringsappen…", "Waiting for the code from your authenticator app…")
                : Lang.T("Loggar in …", "Signing in …");
        }
        else
        {
            state = Lang.T("Inte inloggad", "Not signed in");
        }
        SetDescription(StreamBimExpander, state);

        Show(StreamBimFormCard, !signedIn);
        Show(StreamBimSignedInCard, signedIn);
        StreamBimRegionBox.IsEnabled = editable;
        StreamBimEmailBox.IsEnabled = editable;
        StreamBimPasswordBox.IsEnabled = editable;
        StreamBimSignInButton.IsEnabled = editable
            && SelectedRegion() is not null
            && StreamBimEmailBox.Text.Trim().Length > 0
            && StreamBimPasswordBox.Password.Length > 0;
        StreamBimSignOutButton.IsEnabled = connected && signedIn;
        ShowFailure(StreamBimError, ProviderNames.StreamBim);
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

        Show(InteraxoSignInButton, account is { SignedIn: false });
        Show(InteraxoSignOutButton, signedIn);
        InteraxoSignInButton.IsEnabled = account is { SignedIn: false, SigningIn: false, Configured: true };
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
    /// already retried stays hidden until the engine reports a new one.
    /// </summary>
    private void ShowFailure(InfoBar bar, string provider)
    {
        var progress = _engine.SignInProgressFor(provider);
        var failed = progress is { State: "failed" }
            && !(_dismissedFailures.TryGetValue(provider, out var dismissed) && ReferenceEquals(dismissed, progress));

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
    private void DismissFailure(string provider)
    {
        if (_engine.SignInProgressFor(provider) is { } progress)
        {
            _dismissedFailures[provider] = progress;
        }
    }

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

    private async void OnAccSignOut(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.SignOutAsync(ProviderNames.Acc),
            Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
    }

    /// <summary>The region the user picked, or the engine's default; null when the list is empty.</summary>
    private string? SelectedRegion() =>
        StreamBimRegionBox.SelectedItem is ComboBoxItem { Tag: string tag } && tag.Length > 0 ? tag : null;

    /// <summary>
    /// The password goes to the engine once. Its box is emptied before the command is built,
    /// so it is not left on screen, and nothing keeps it after the command has gone.
    /// </summary>
    private async void OnStreamBimSignIn(object sender, RoutedEventArgs args)
    {
        var region = SelectedRegion();
        var username = StreamBimEmailBox.Text.Trim();
        var password = StreamBimPasswordBox.Password;
        StreamBimPasswordBox.Password = string.Empty;
        if (region is null || username.Length == 0 || password.Length == 0)
        {
            RefreshAccounts();
            return;
        }

        DismissFailure(ProviderNames.StreamBim);
        RefreshAccounts();
        var ack = await _engine.SignInStreamBimAsync(region, username, password);
        ReportIfFailed(ack, Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        RefreshAccounts();
    }

    private async void OnStreamBimSignOut(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.SignOutAsync(ProviderNames.StreamBim),
            Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
    }

    private void OnStreamBimRegionChanged(object sender, SelectionChangedEventArgs args)
    {
        if (!_populatingRegions && SelectedRegion() is { } region)
        {
            _streamBimRegionChoice = region;
        }
        RefreshAccounts();
    }

    private void OnStreamBimFieldChanged(object sender, TextChangedEventArgs args) => RefreshAccounts();

    private void OnStreamBimPasswordChanged(object sender, RoutedEventArgs args) => RefreshAccounts();

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
    /// Fills the region list. The user's choice stays when the list still has it; otherwise the
    /// engine's default (is_default) is preselected.
    /// </summary>
    private void ShowStreamBimRegions(StreamBimRegionsInfo? info)
    {
        var regions = (info?.Regions ?? Array.Empty<StreamBimRegion>())
            .Where(region => region is not null && !string.IsNullOrWhiteSpace(region.Subdomain))
            .ToList();
        var fallback = regions.FirstOrDefault(region => region.IsDefault) ?? regions.FirstOrDefault();
        var keep = _streamBimRegionChoice is { } chosen && regions.Any(region => region.Subdomain == chosen)
            ? chosen
            : fallback?.Subdomain;

        _populatingRegions = true;
        try
        {
            StreamBimRegionBox.Items.Clear();
            ComboBoxItem? selected = null;
            foreach (var region in regions)
            {
                var item = new ComboBoxItem
                {
                    Content = string.IsNullOrWhiteSpace(region.Title) ? region.Subdomain : region.Title,
                    Tag = region.Subdomain,
                };
                StreamBimRegionBox.Items.Add(item);
                if (region.Subdomain == keep)
                {
                    selected = item;
                }
            }
            StreamBimRegionBox.SelectedItem = selected;
        }
        finally
        {
            _populatingRegions = false;
        }

        RefreshAccounts();
    }

    /// <summary>
    /// The StreamBIM MFA dialog: six digits from the authenticator app. Verify sends the code;
    /// Cancel (or closing the dialog) cancels the sign-in in the engine.
    /// </summary>
    private async Task ShowMfaDialogAsync()
    {
        if (_mfaDialog is not null || XamlRoot is not { } root)
        {
            return;
        }

        _mfaEndedByEngine = false;
        var code = new TextBox
        {
            MaxLength = 6,
            PlaceholderText = "000000",
            IsSpellCheckEnabled = false,
            IsTextPredictionEnabled = false,
        };
        AutomationProperties.SetName(code, Lang.T("Verifieringskod", "Verification code"));

        var body = new StackPanel { Spacing = 8 };
        body.Children.Add(Line(Lang.T(
            "Skriv in de sex siffrorna från din autentiseringsapp för StreamBIM.",
            "Enter the six digits from your authenticator app for StreamBIM.")));
        body.Children.Add(code);

        var dialog = new ContentDialog
        {
            XamlRoot = root,
            // The dialog hangs in the popup root and does not inherit the theme choice.
            RequestedTheme = MainWindow.CurrentTheme,
            Title = Lang.T("Ange koden från din autentiseringsapp", "Enter the code from your authenticator app"),
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
            dialog.IsPrimaryButtonEnabled = digits.Length == 6;
        };

        _mfaDialog = dialog;
        ContentDialogResult answer;
        try
        {
            answer = await dialog.ShowAsync();
        }
        catch (Exception)
        {
            // Another dialog is open, or the window is closing. Nothing can be asked: the sign-in is cancelled.
            _mfaDialog = null;
            _ = await _engine.CancelSignInAsync(ProviderNames.StreamBim);
            MainWindow.Instance?.ShowMessage(
                Lang.T("Kodfönstret kunde inte visas", "The code dialog could not be shown"),
                Lang.T("Logga in på StreamBIM igen för att försöka på nytt.", "Sign in to StreamBIM again to try again."),
                InfoBarSeverity.Warning);
            return;
        }

        _mfaDialog = null;
        if (_mfaEndedByEngine)
        {
            // The engine ended the wait (signed in, failed or cancelled) and closed the dialog itself.
            RefreshAccounts();
            return;
        }

        if (answer == ContentDialogResult.Primary)
        {
            ReportIfFailed(await _engine.SignInMfaAsync(ProviderNames.StreamBim, code.Text),
                Lang.T("Koden gick inte att skicka", "The code could not be sent"));
        }
        else
        {
            ReportIfFailed(await _engine.CancelSignInAsync(ProviderNames.StreamBim),
                Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        }
        RefreshAccounts();
    }

    /// <summary>Every sign-in message: the MFA dialog opens or closes, and the cards take the new state.</summary>
    private void OnSignInProgress(SignInProgressInfo progress)
    {
        if (progress.Provider == ProviderNames.StreamBim)
        {
            switch (progress.State)
            {
                case "mfa_required":
                    _ = ShowMfaDialogAsync();
                    break;

                case "signed_in":
                case "failed":
                case "cancelled":
                    if (_mfaDialog is not null)
                    {
                        _mfaEndedByEngine = true;
                        _mfaDialog.Hide();
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
    /// secret box is emptied once it has been read: it is sent once, never kept on screen.
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
        InteraxoSecretBox.Password = string.Empty;

        var ack = await _engine.SetSettingAsync("interaxo_client_id", clientId);
        if (ack.Ok)
        {
            ack = await _engine.SetSettingAsync("interaxo_redirect_uri", redirect);
        }
        if (ack.Ok && secret.Length > 0)
        {
            ack = await _engine.SetSettingAsync("interaxo_client_secret", secret);
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
