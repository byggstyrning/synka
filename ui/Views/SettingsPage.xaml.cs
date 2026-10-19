using System;
using System.ComponentModel;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Synka.Services;
using Microsoft.UI.Xaml;
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

    /// <summary>Sign-in was started and the engine does not yet report an account.</summary>
    private bool _signInStarted;

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
        _engine.Notified += OnApsClientIdNotified;
        RefreshAccount();
        RefreshSettings();
        RefreshEnabled();
        ShowUpdate();
    }

    private void OnUnloaded(object sender, RoutedEventArgs args)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        _engine.PropertyChanged -= OnEngineChanged;
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

            case nameof(EngineClient.Status):
                RefreshAccount();
                break;

            case nameof(EngineClient.IsConnected):
            case nameof(EngineClient.IsSignedIn):
                RefreshAccount();
                RefreshEnabled();
                ShowUpdate();
                break;
        }
    }

    private void RefreshAccount()
    {
        var status = _engine.IsConnected ? _engine.Status : null;
        if (status is { SignedIn: true })
        {
            _signInStarted = false;
        }

        AccountText.Text = !_engine.IsConnected
            ? Lang.T("Ingen kontakt med motorn.", "No contact with the engine.")
            : status is null
                ? Lang.T("Hämtar kontot …", "Fetching the account …")
                : status.SignedIn
                    ? AccountLabel(status)
                    : _signInStarted
                        ? Lang.T("Slutför inloggningen i webbläsaren.", "Finish signing in in the browser.")
                        : Lang.T("Inte inloggad.", "Not signed in.");
        SignInButton.Visibility = status is { SignedIn: false } ? Visibility.Visible : Visibility.Collapsed;
        SignOutButton.Visibility = status is { SignedIn: true } ? Visibility.Visible : Visibility.Collapsed;

        RefreshFolder();
        RefreshApsClientIdControls();
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
        }
        finally
        {
            _mirroring = false;
        }
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
        SignInButton.IsEnabled = connected;
        SignOutButton.IsEnabled = connected;
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

    private static string AccountLabel(StatusInfo status)
    {
        var parts = new[] { status.User?.Name, status.User?.Email }
            .Where(part => !string.IsNullOrWhiteSpace(part));
        var label = string.Join(" · ", parts);
        return label.Length > 0 ? label : Lang.T("Inloggad.", "Signed in.");
    }

    // ----------------------------------------------------------- account, folder

    private async void OnSignIn(object sender, RoutedEventArgs args)
    {
        var ack = await _engine.SignInAsync();
        if (ack.Ok)
        {
            // The answer is a `status` once the browser is done; until then the account line says what to do.
            _signInStarted = true;
            RefreshAccount();
        }
        else
        {
            ReportIfFailed(ack, Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        }
    }

    private async void OnSignOut(object sender, RoutedEventArgs args)
    {
        ReportIfFailed(await _engine.SignOutAsync(), Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
    }

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
        SignInButton.IsEnabled = connected && !_apsClientIdEdit.IsWaiting;

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
            "Alla projekt byggs om där från ACC. Filerna laddas ner igen när du öppnar dem.",
            "Every project is rebuilt there from ACC. The files are downloaded again when you open them.")));
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
    /// Every project rebuilt from ACC where it is, once the user has said yes. Each
    /// project reports in its own notify, so nothing more is shown here on success.
    /// </summary>
    private async void OnRebuildAll(object sender, RoutedEventArgs args)
    {
        var body = new StackPanel { Spacing = 8 };
        body.Children.Add(Line(Lang.T(
            "Varje projekt byggs om från ACC där det ligger. Filerna laddas ner igen när du öppnar dem. Ändringar som inte hunnit synkas sparas först i \"Synka - återställda filer\".",
            "Every project is rebuilt from ACC where it is. The files are downloaded again when you open them. Changes that have not been synced yet are first saved in \"Synka - återställda filer\".")));

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
