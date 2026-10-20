using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using Synka.Services;
using Synka.Views;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Windows.Storage;

namespace Synka;

public sealed partial class MainWindow : Window
{
    /// <summary>The navigation tags (the Tag of each NavigationViewItem in the XAML).</summary>
    public const string OverviewTag = "overview";

    public const string ProjectsTag = "projects";

    public const string ActivityTag = "activity";

    public const string AboutTag = "about";

    /// <summary>The settings gear: NavigationView's own entry, not one of the tagged items.</summary>
    public const string SettingsTag = "settings";

    /// <summary>At most this many messages stand in the bar area; the oldest goes first.</summary>
    private const int MaxNotices = 3;

    /// <summary>Information and success messages close by themselves after this long.</summary>
    private static readonly TimeSpan NoticeLifetime = TimeSpan.FromSeconds(10);

    /// <summary>
    /// The engine bar waits this long before it opens, so a short gap (the window just
    /// opened, the engine restarting) does not flash a warning and push the page down.
    /// </summary>
    private static readonly TimeSpan EngineBarDelay = TimeSpan.FromSeconds(1.5);

    /// <summary>
    /// The frame that carries the pages. A field of its own, because the frame is
    /// REPLACED on a language change (see <see cref="OnLanguageChanged"/>).
    /// </summary>
    private Frame _frame;

    /// <summary>The update the user has closed the toast for. Not shown again for that version.</summary>
    private string? _dismissedVersion;

    /// <summary>Install was pressed in the toast and the engine has not reported since.</summary>
    private bool _installRequested;

    private DispatcherQueueTimer? _engineBarTimer;

    /// <summary>The open window. The settings page reaches the theme through it.</summary>
    public static MainWindow? Instance { get; private set; }

    public MainWindow()
    {
        InitializeComponent();
        try
        {
            // The XAML TitleBar icon is content; Windows also needs the native HWND icon.
            AppWindow.SetIcon(System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "app.ico"));
        }
        catch (Exception exception)
        {
            DiagnosticLog.Write("fönsterikon", exception);
        }
        Instance = this;
        _frame = PageFrame;

        // Both lines belong together: without ExtendsContentIntoTitleBar SetTitleBar
        // does nothing. Set in the constructor, before the window shows.
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(AppTitleBar);

        RootGrid.Loaded += OnRootLoaded;

        // The theme before the window shows, or a wrong theme flashes by.
        ApplyTheme(LoadTheme());

        // The language before the first page is navigated.
        ApplyLanguage();
        Navigation.Loaded += (_, _) => ApplyLanguage();
        Lang.Changed += OnLanguageChanged;

        Title = "Synka";
        ApplyBadgeStyle();

        _frame.Navigate(typeof(OverviewPage));

        var engine = EngineClient.Instance;
        engine.PropertyChanged += OnEngineChanged;
        engine.Notified += OnNotified;
        UpdateEngineView();
        RefreshUpdateToast();

        Closed += OnWindowClosed;
    }

    private void OnWindowClosed(object sender, WindowEventArgs args)
    {
        var engine = EngineClient.Instance;
        engine.PropertyChanged -= OnEngineChanged;
        engine.Notified -= OnNotified;
        Lang.Changed -= OnLanguageChanged;
        _engineBarTimer?.Stop();
    }

    /// <summary>
    /// Brings the window forward: another start of Synka.exe (the tray icon, the
    /// Start menu) is redirected here instead of opening a second window.
    /// </summary>
    public void BringToFront()
    {
        try
        {
            if (AppWindow.Presenter is OverlappedPresenter { State: OverlappedPresenterState.Minimized } presenter)
            {
                presenter.Restore();
            }

            Activate();
            SetForegroundWindow(WinRT.Interop.WindowNative.GetWindowHandle(this));
        }
        catch (Exception exception)
        {
            DiagnosticLog.Write("fönstret fram", exception);
        }
    }

    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(IntPtr window);

    // ---------------------------------------------------------------- engine

    /// <summary>What the title bar subtitle, the badge and the engine InfoBar say.</summary>
    private void OnEngineChanged(object? sender, PropertyChangedEventArgs args)
    {
        switch (args.PropertyName)
        {
            case nameof(EngineClient.IsConnected):
                UpdateEngineView();
                RefreshUpdateToast();
                break;

            case nameof(EngineClient.ConnectionState):
            case nameof(EngineClient.Status):
                UpdateEngineView();
                break;

            case nameof(EngineClient.Update):
                _installRequested = false;
                RefreshUpdateToast();
                break;
        }
    }

    private void OnNotified(object? sender, NotifyInfo notice)
    {
        if (ErrorDisplay.TryUnsupportedFileType(notice.Body, out _))
        {
            ShowMessage(
                Lang.T("Filtyp stöds inte i ACC", "File type not supported by ACC"),
                ErrorDisplay.FriendlyDetail(notice.Body),
                InfoBarSeverity.Informational,
                Lang.T("Filtyper i ACC", "ACC file types"),
                () => _ = Windows.System.Launcher.LaunchUriAsync(ErrorDisplay.AccFileTypesHelp));
            return;
        }
        var severity = notice.Severity switch
        {
            "error" => InfoBarSeverity.Error,
            "warning" => InfoBarSeverity.Warning,
            _ => InfoBarSeverity.Informational,
        };
        ShowMessage(notice.Title ?? string.Empty, ErrorDisplay.FriendlyDetail(notice.Body), severity);
    }

    /// <summary>
    /// Shows a message in the bar area above the page. Used for engine notices and for
    /// the results of actions. Newest on top, at most three; the same message twice
    /// stays one bar. Information closes by itself after a while, unless it offers an
    /// action; warnings and errors stay until closed.
    /// </summary>
    public void ShowMessage(string title, string body, InfoBarSeverity severity, string? actionText = null, Action? action = null)
    {
        // The engine's notices are all titled with the app name (docs\IPC.md), which the
        // title bar already says. The message alone is clearer.
        var shownTitle = string.Equals(title, AppInfo.Name, StringComparison.Ordinal) ? string.Empty : title;

        foreach (var open in NotifyPanel.Children.OfType<InfoBar>())
        {
            if (open.Severity == severity && open.Title == shownTitle && open.Message == body)
            {
                return;
            }
        }

        var bar = new InfoBar
        {
            Title = shownTitle,
            Message = body,
            Severity = severity,
            IsClosable = true,
            IsOpen = true,
        };

        if (actionText is not null && action is not null)
        {
            var button = new Button { Content = actionText };
            button.Click += (_, _) =>
            {
                RemoveNotice(bar);
                action();
            };
            bar.ActionButton = button;
        }

        bar.Closed += OnNoticeClosed;

        if (action is null && severity is InfoBarSeverity.Informational or InfoBarSeverity.Success)
        {
            var timer = DispatcherQueue.CreateTimer();
            timer.Interval = NoticeLifetime;
            timer.IsRepeating = false;
            timer.Tick += (sender, _) =>
            {
                sender.Stop();
                RemoveNotice(bar);
            };

            // Kept on the bar, so the timer lives exactly as long as the bar does.
            bar.Tag = timer;
            timer.Start();
        }

        NotifyPanel.Children.Insert(0, bar);
        while (NotifyPanel.Children.Count > MaxNotices)
        {
            if (NotifyPanel.Children[NotifyPanel.Children.Count - 1] is InfoBar oldest)
            {
                RemoveNotice(oldest);
            }
            else
            {
                NotifyPanel.Children.RemoveAt(NotifyPanel.Children.Count - 1);
            }
        }

        UpdateBarHost();
    }

    private void OnNoticeClosed(InfoBar sender, InfoBarClosedEventArgs args)
    {
        RemoveNotice(sender);
    }

    private void RemoveNotice(InfoBar bar)
    {
        if (bar.Tag is DispatcherQueueTimer timer)
        {
            timer.Stop();
            bar.Tag = null;
        }

        bar.Closed -= OnNoticeClosed;
        NotifyPanel.Children.Remove(bar);
        UpdateBarHost();
    }

    private void OnClearNotices(object sender, RoutedEventArgs args)
    {
        foreach (var bar in NotifyPanel.Children.OfType<InfoBar>().ToArray())
        {
            RemoveNotice(bar);
        }
    }

    private void OnBarClosed(InfoBar sender, InfoBarClosedEventArgs args)
    {
        UpdateBarHost();
    }

    /// <summary>The bar row takes room only while a bar is open.</summary>
    private void UpdateBarHost()
    {
        ClearNoticesButton.Visibility = NotifyPanel.Children.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        var any = EngineBar.IsOpen
            || ShortcutBar.IsOpen
            || HeldDeletesPanel.Children.Count > 0
            || NotifyPanel.Children.Count > 0;
        BarHost.Visibility = any ? Visibility.Visible : Visibility.Collapsed;
    }

    // --------------------------------------------------------- held deletes

    /// <summary>The held-delete bars on screen, by project id.</summary>
    private readonly Dictionary<string, InfoBar> _heldBars = new();

    /// <summary>
    /// One warning bar per burst of deletes the engine holds back (mass-delete
    /// protection), on every page, until the user answers. Status comes twice a
    /// second, so bars are kept and only their text is updated.
    /// </summary>
    private void UpdateHeldDeletes()
    {
        var engine = EngineClient.Instance;
        var wanted = new Dictionary<string, HeldDeleteInfo>();
        if (engine.IsConnected)
        {
            foreach (var entry in engine.Status?.HeldDeletes ?? new List<HeldDeleteInfo>())
            {
                // Without a project id the engine cannot be answered, so there is nothing to offer.
                if (entry is { ProjectId: { Length: > 0 } id, Count: > 0 })
                {
                    wanted[id] = entry;
                }
            }
        }

        foreach (var gone in _heldBars.Keys.Where(id => !wanted.ContainsKey(id)).ToList())
        {
            HeldDeletesPanel.Children.Remove(_heldBars[gone]);
            _heldBars.Remove(gone);
        }

        foreach (var (id, entry) in wanted)
        {
            var project = string.IsNullOrWhiteSpace(entry.Project) ? Lang.T("projektet", "the project") : entry.Project;
            var count = Fmt.Count(entry.Count);
            var message = Lang.T(
                $"Du tog bort {count} objekt i {project}. De tas inte bort i molnet förrän du bekräftar.",
                $"You deleted {count} items in {project}. They are not deleted in the cloud until you confirm.");

            if (_heldBars.TryGetValue(id, out var shown))
            {
                if (shown.Message != message)
                {
                    shown.Message = message;
                }
                continue;
            }

            var bar = BuildHeldDeletesBar(id, project, message);
            _heldBars[id] = bar;
            HeldDeletesPanel.Children.Add(bar);
        }

        UpdateBarHost();
    }

    private InfoBar BuildHeldDeletesBar(string projectId, string project, string message)
    {
        var bar = new InfoBar
        {
            Severity = InfoBarSeverity.Warning,
            Title = Lang.T("Borttagningar väntar", "Deletions on hold"),
            Message = message,
            IsClosable = false,
            IsOpen = true,
        };

        var delete = new Button { Content = Lang.T("Ta bort i molnet", "Delete in the cloud") };
        AutomationProperties.SetName(delete, Lang.T($"Ta bort i molnet, {project}", $"Delete in the cloud, {project}"));
        var restore = new Button { Content = Lang.T("Återställ", "Restore") };
        AutomationProperties.SetName(restore, Lang.T($"Återställ filerna, {project}", $"Restore the files, {project}"));

        delete.Click += async (_, _) => await AnswerHeldDeletesAsync(projectId, true, delete, restore);
        restore.Click += async (_, _) => await AnswerHeldDeletesAsync(projectId, false, delete, restore);

        // Two answers, so they go in the bar's content (ActionButton holds one). The
        // margins line them up with the message above (InfoBar's own template).
        var buttons = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 8,
            Margin = new Thickness(0, 0, 16, 12),
        };
        buttons.Children.Add(delete);
        buttons.Children.Add(restore);
        bar.Content = buttons;
        return bar;
    }

    /// <summary>
    /// Sends the answer. The bar goes when the engine's next status no longer holds
    /// the burst; until then its buttons wait. A refusal gives the buttons back.
    /// </summary>
    private async Task AnswerHeldDeletesAsync(string projectId, bool confirm, Button delete, Button restore)
    {
        delete.IsEnabled = false;
        restore.IsEnabled = false;

        var ack = await EngineClient.Instance.ConfirmDeletesAsync(projectId, confirm);
        if (ack.Ok)
        {
            // Accepted. Should the engine still hold the burst a while later, the
            // buttons come back rather than leave the user without an answer.
            await Task.Delay(TimeSpan.FromSeconds(15));
            delete.IsEnabled = true;
            restore.IsEnabled = true;
        }
        else
        {
            delete.IsEnabled = true;
            restore.IsEnabled = true;
            ShowMessage(
                confirm
                    ? Lang.T("Borttagningen gick inte att bekräfta", "The deletion could not be confirmed")
                    : Lang.T("Filerna gick inte att återställa", "The files could not be restored"),
                ack.Error ?? Lang.T("Okänt fel.", "Unknown error."),
                InfoBarSeverity.Error);
        }
    }

    /// <summary>
    /// The engine InfoBar, the title bar subtitle and the activity badge. Connected:
    /// the subtitle says the engine's state and queue. Not connected: the InfoBar
    /// says what is being done about it, and offers to start the engine when nothing is.
    /// </summary>
    private void UpdateEngineView()
    {
        var engine = EngineClient.Instance;
        UpdateSubtitle();
        UpdateBadge();
        UpdateHeldDeletes();

        if (engine.IsConnected)
        {
            _engineBarTimer?.Stop();
            if (EngineBar.IsOpen)
            {
                EngineBar.IsOpen = false;
                UpdateBarHost();
            }
            return;
        }

        FillEngineBar(engine.ConnectionState);
        if (EngineBar.IsOpen)
        {
            return;
        }

        if (_engineBarTimer is null)
        {
            _engineBarTimer = DispatcherQueue.CreateTimer();
            _engineBarTimer.Interval = EngineBarDelay;
            _engineBarTimer.IsRepeating = false;
            _engineBarTimer.Tick += OnEngineBarTimer;
        }

        if (!_engineBarTimer.IsRunning)
        {
            _engineBarTimer.Start();
        }
    }

    private void OnEngineBarTimer(DispatcherQueueTimer sender, object args)
    {
        sender.Stop();
        var engine = EngineClient.Instance;
        if (!engine.IsConnected)
        {
            FillEngineBar(engine.ConnectionState);
            EngineBar.IsOpen = true;
            UpdateBarHost();
        }
    }

    private void FillEngineBar(EngineConnectionState state)
    {
        // A restart after a sync root move is expected: said as what it is, not as a lost contact.
        EngineBar.Title = state == EngineConnectionState.Restarting
            ? Lang.T("Motorn startar om", "The engine is restarting")
            : Lang.T("Ingen kontakt med motorn", "No contact with the engine");
        EngineBar.Message = state switch
        {
            EngineConnectionState.Restarting => Lang.T(
                "Synkmappen flyttas. Fönstret ansluter igen när motorn är tillbaka …",
                "The sync folder is being moved. The window reconnects once the engine is back …"),
            EngineConnectionState.Starting => Lang.T(
                $"Startar {AppInfo.Name}-motorn …", $"Starting the {AppInfo.Name} engine …"),
            EngineConnectionState.Disconnected => Lang.T(
                $"Tappade kontakten med {AppInfo.Name}-motorn. Försöker igen …",
                $"Lost contact with the {AppInfo.Name} engine. Trying again …"),
            EngineConnectionState.NotRunning => Lang.T(
                $"{AppInfo.Name}-motorn körs inte. Synken står still tills den startar.",
                $"The {AppInfo.Name} engine is not running. The sync stands still until it starts."),
            _ => Lang.T("Ansluter till motorn …", "Connecting to the engine …"),
        };

        var canStart = state is EngineConnectionState.Disconnected or EngineConnectionState.NotRunning;
        EngineBar.Severity = canStart ? InfoBarSeverity.Warning : InfoBarSeverity.Informational;
        EngineBarStartButton.Content = Lang.T("Starta motorn", "Start the engine");
        EngineBarStartButton.Visibility = canStart ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnStartEngine(object sender, RoutedEventArgs args)
    {
        EngineClient.Instance.StartEngine();
    }

    /// <summary>"väntar på synk · 3 i kö": the state and the queue, visible from every page.</summary>
    private void UpdateSubtitle()
    {
        var engine = EngineClient.Instance;
        string subtitle;
        if (!engine.IsConnected)
        {
            subtitle = Lang.T("ingen kontakt med motorn", "no contact with the engine");
        }
        else if (engine.Status is not { } status)
        {
            subtitle = Lang.T("ansluten", "connected");
        }
        else
        {
            var parts = new List<string> { StateLabel(status.State) };
            if (status.PendingJobs > 0)
            {
                var pending = Fmt.Count(status.PendingJobs);
                parts.Add(Lang.T($"{pending} i kö", $"{pending} queued"));
            }

            if (status.FailedJobs > 0)
            {
                var failed = Fmt.Count(status.FailedJobs);
                parts.Add(Lang.T($"{failed} {(status.FailedJobs == 1 ? "misslyckad" : "misslyckade")}",
                    $"{failed} failed"));
            }

            subtitle = string.Join(" · ", parts);
        }

        if (AppTitleBar.Subtitle != subtitle)
        {
            AppTitleBar.Subtitle = subtitle;
        }
    }

    /// <summary>The failed jobs as a number on Aktivitet, so a failure is seen from any page.</summary>
    private void UpdateBadge()
    {
        var engine = EngineClient.Instance;
        var failed = engine.IsConnected ? engine.Status?.FailedJobs ?? 0 : 0;
        if (failed > 0)
        {
            ActivityBadge.Value = (int)Math.Min(failed, int.MaxValue);
            ActivityBadge.Visibility = Visibility.Visible;
        }
        else
        {
            ActivityBadge.Visibility = Visibility.Collapsed;
        }

        var label = Lang.T("Aktivitet", "Activity");
        AutomationProperties.SetName(
            ActivityNavItem,
            failed > 0 ? Lang.T($"{label}, {failed} misslyckade", $"{label}, {failed} failed") : label);
    }

    /// <summary>
    /// The badge in the colour of a failure. Looked up in code: a style key that
    /// does not exist in this version of WinUI then leaves the default colour,
    /// where in XAML it would stop the window from loading.
    /// </summary>
    private void ApplyBadgeStyle()
    {
        try
        {
            if (Application.Current.Resources["CriticalValueInfoBadgeStyle"] is Style style)
            {
                ActivityBadge.Style = style;
            }
        }
        catch (Exception)
        {
            // Not there: the accent-coloured default badge says the same.
        }
    }

    /// <summary>The engine's state word, in the app's language. The engine's own text is not used here.</summary>
    public static string StateLabel(string? state) => state switch
    {
        "signed_out" => Lang.T("ej inloggad", "signed out"),
        "synced" => Lang.T("synkad", "synced"),
        "syncing" => Lang.T("synkar", "syncing"),
        "waiting" => Lang.T("väntar på synk", "waiting to sync"),
        "attention" => Lang.T("behöver ses över", "needs attention"),
        "offline" => Lang.T("offline", "offline"),
        "error" => Lang.T("fel", "error"),
        null => Lang.T("ansluten", "connected"),
        _ => state,
    };

    // ---------------------------------------------------------------- update

    /// <summary>
    /// The update toast. Shown when the engine reports a newer version and the user
    /// has not closed it for that version, and always while the engine installs one
    /// (on request, or by itself when idle). Installera nu asks the engine to install.
    /// </summary>
    private void RefreshUpdateToast()
    {
        var engine = EngineClient.Instance;
        var update = engine.Update;
        var installing = update?.Installing == true;

        if (update is null
            || (!update.Available && !installing)
            || (!installing && !_installRequested && update.Version is not null && update.Version == _dismissedVersion))
        {
            HideUpdateToast();
            return;
        }

        var version = update.Version;
        if (installing || _installRequested)
        {
            UpdateToastText.Text = installing
                ? version is null
                    ? Lang.T($"Uppdaterar {AppInfo.Name} …", $"Updating {AppInfo.Name} …")
                    : Lang.T($"Uppdaterar till {AppInfo.Name} {version} …", $"Updating to {AppInfo.Name} {version} …")
                : Lang.T("Installerar …", "Installing …");
            UpdateToastButton.Visibility = Visibility.Collapsed;
        }
        else
        {
            UpdateToastText.Text = update.Error is { } error
                ? error
                : version is null
                    ? Lang.T($"Det finns en ny version av {AppInfo.Name}.", $"A new version of {AppInfo.Name} is out.")
                    : Lang.T($"Ny version — {AppInfo.Name} {version} finns.", $"New version — {AppInfo.Name} {version} is out.");
            UpdateToastButton.Content = update.Error is null
                ? Lang.T("Installera nu", "Install now")
                : Lang.T("Försök igen", "Try again");
            UpdateToastButton.Visibility = Visibility.Visible;
            UpdateToastButton.IsEnabled = engine.IsConnected;
        }

        var notNow = Lang.T("Inte nu", "Not now");
        ToolTipService.SetToolTip(UpdateToastCloseButton, notNow);
        AutomationProperties.SetName(UpdateToastCloseButton, notNow);
        UpdateToast.Visibility = Visibility.Visible;
    }

    private void HideUpdateToast()
    {
        UpdateToast.Visibility = Visibility.Collapsed;
    }

    private async void OnUpdateToastUpdate(object sender, RoutedEventArgs args)
    {
        _installRequested = true;
        RefreshUpdateToast();

        var ack = await EngineClient.Instance.InstallUpdateAsync();
        if (!ack.Ok)
        {
            _installRequested = false;
            RefreshUpdateToast();
            UpdateToastText.Text = ack.Error ?? Lang.T("Uppdateringen gick inte att starta.", "The update could not be started.");
            UpdateToastButton.Content = Lang.T("Försök igen", "Try again");
        }
    }

    private void OnUpdateToastClose(object sender, RoutedEventArgs args)
    {
        _dismissedVersion = EngineClient.Instance.Update?.Version;
        HideUpdateToast();
    }

    // ------------------------------------------------------------- shortcut

    /// <summary>
    /// Offers the desktop shortcut once. Called by App.OnLaunched after the first-run
    /// notice has been accepted, so the two dialogs never open together.
    /// </summary>
    public async void OfferDesktopShortcutIfNeeded()
    {
        if (!DesktopShortcut.ShouldOffer())
        {
            return;
        }

        // Marked BEFORE the dialog is shown, whatever the answer: asking exactly once
        // is a promise to the user.
        DesktopShortcut.MarkOffered();

        var dialog = new ContentDialog
        {
            XamlRoot = _frame.XamlRoot,
            // The dialog hangs in the popup root and does not inherit the theme choice.
            RequestedTheme = RootGrid.RequestedTheme,
            Title = Lang.T("Genväg på skrivbordet?", "Shortcut on the desktop?"),
            Content = Lang.T(
                $"Vill du ha en genväg till {AppInfo.Name} på skrivbordet? Frågan ställs bara den här gången.",
                $"Would you like a shortcut to {AppInfo.Name} on your desktop? You will only be asked this once."),
            PrimaryButtonText = Lang.T("Skapa genväg", "Create shortcut"),
            CloseButtonText = Lang.T("Nej tack", "No thanks"),
            DefaultButton = ContentDialogButton.Primary,
        };

        try
        {
            if (await dialog.ShowAsync() != ContentDialogResult.Primary)
            {
                return;
            }
        }
        catch (Exception)
        {
            // The window closed before the dialog opened. A missed question is nothing to crash over.
            return;
        }

        if (DesktopShortcut.TryCreate() is { } trouble)
        {
            ShortcutBar.Title = Lang.T("Ingen genväg skapad", "No shortcut created");
            ShortcutBar.Message = trouble;
            ShortcutBar.IsOpen = true;
            UpdateBarHost();
        }
    }

    // ------------------------------------------------------- first-run notice

    /// <summary>
    /// Shows the first-run notice until it is accepted. Returns true when accepted;
    /// false when the user quits instead (the caller then exits without starting the engine).
    /// ContentDialog needs a XamlRoot, which exists only once the window is loaded.
    /// </summary>
    public async Task<bool> EnsureNoticeAcceptedAsync()
    {
        if (FirstRunNotice.IsAccepted())
        {
            return true;
        }

        if (!RootGrid.IsLoaded)
        {
            var loaded = new TaskCompletionSource();
            void OnLoaded(object sender, RoutedEventArgs args)
            {
                RootGrid.Loaded -= OnLoaded;
                loaded.TrySetResult();
            }

            RootGrid.Loaded += OnLoaded;
            await loaded.Task;
        }

        while (true)
        {
            var dialog = new ContentDialog
            {
                XamlRoot = RootGrid.XamlRoot,
                // The dialog hangs in the popup root and does not inherit the theme choice.
                RequestedTheme = RootGrid.RequestedTheme,
                Title = Lang.T("Innan du börjar", "Before you start"),
                Content = NoticeContent(),
                PrimaryButtonText = Lang.T("Jag förstår och godkänner", "I understand and accept"),
                SecondaryButtonText = Lang.IsEnglish ? "På svenska" : "In English",
                CloseButtonText = Lang.T("Avsluta", "Quit"),
                DefaultButton = ContentDialogButton.Primary,
            };

            ContentDialogResult result;
            try
            {
                result = await dialog.ShowAsync();
            }
            catch (Exception exception)
            {
                DiagnosticLog.Write("villkorsdialog", exception);
                return false;
            }

            switch (result)
            {
                case ContentDialogResult.Primary:
                    FirstRunNotice.RecordAcceptance();
                    return true;

                case ContentDialogResult.Secondary:
                    // Same notice, other language: the dialog is built again with the new texts.
                    Lang.Change(Lang.IsEnglish ? AppLanguage.Swedish : AppLanguage.English);
                    continue;

                default:
                    return false;
            }
        }
    }

    /// <summary>The body of the first-run notice. Both languages stand in place, as everywhere in the window.</summary>
    private static ScrollViewer NoticeContent()
    {
        var stack = new StackPanel { Spacing = 10 };
        stack.Children.Add(NoticeParagraph(
            "Synka är en fristående produkt från Byggstyrning, inte en Autodesk-produkt. Autodesks support kan inte hjälpa till med Synka.",
            "Synka is an independent product of Byggstyrning, not an Autodesk product. Autodesk support cannot help with Synka."));
        stack.Children.Add(NoticeParagraph(
            "Administratören för varje ACC-konto (hub) som ska synkas måste godkänna Synkas Client ID som Custom Integration – även för hubbar som ägs av en beställare eller annan organisation.",
            "The account admin of every ACC hub to be synced must approve Synka's Client ID as a Custom Integration – including hubs owned by a client or another organisation."));
        stack.Children.Add(NoticeParagraph(
            "ACC är huvudkopian. Synka kopierar filer mellan ACC och den här datorn, och data kan gå förlorad. Kontrollera viktiga ändringar i ACC.",
            "ACC holds the master copy. Synka copies files between ACC and this PC, and data loss is possible. Check important changes in ACC."));
        stack.Children.Add(NoticeParagraph(
            "Du är fortfarande bunden av Autodesks egna villkor för ACC och ditt Autodesk-konto.",
            "You remain bound by Autodesk's own terms for ACC and your Autodesk account."));
        stack.Children.Add(NoticeParagraph(
            "Synka skickar filinnehåll bara mellan den här datorn och Autodesk. Uppdateringskontrollen via internet är avstängd.",
            "Synka sends file content only between this PC and Autodesk. The online update check is off."));
        stack.Children.Add(NoticeParagraph(
            "Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma och Revit är registrerade varumärken eller varumärken som tillhör Autodesk, Inc. och/eller dess dotter- eller närstående bolag. Synka är en fristående produkt från Byggstyrning och är inte ansluten till, godkänd av eller sponsrad av Autodesk.",
            "Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma and Revit are registered trademarks or trademarks of Autodesk, Inc., and/or its subsidiaries and/or affiliates. Synka is an independent product of Byggstyrning and is not affiliated with, endorsed by or sponsored by Autodesk.",
            caption: true));

        return new ScrollViewer { MaxHeight = 440, Content = stack };
    }

    private static TextBlock NoticeParagraph(string sv, string en, bool caption = false)
    {
        var text = new TextBlock { Text = Lang.T(sv, en), TextWrapping = TextWrapping.Wrap };
        if (caption)
        {
            text.Style = (Style)Application.Current.Resources["CaptionTextBlockStyle"];
        }

        return text;
    }

    // --------------------------------------------------------------- language

    /// <summary>
    /// The window's own texts in the chosen language. The navigation and the bars
    /// live in the window, which is built only once, so they are set here. The
    /// pages read their language when built, and are rebuilt by <see cref="OnLanguageChanged"/>.
    /// </summary>
    private void ApplyLanguage()
    {
        OverviewNavItem.Content = Lang.T("Översikt", "Overview");
        ToolTipService.SetToolTip(OverviewNavItem, Lang.T(
            "Synkstatus och pågående överföringar.",
            "Sync status and transfers in progress."));

        ProjectsNavItem.Content = Lang.T("Projekt", "Projects");
        ToolTipService.SetToolTip(ProjectsNavItem, Lang.T(
            "Välj vilka projekt som ska synkas till datorn.",
            "Choose which projects are synced to this computer."));

        ActivityNavItem.Content = Lang.T("Aktivitet", "Activity");
        ToolTipService.SetToolTip(ActivityNavItem, Lang.T(
            "Det senaste som hänt i synken.",
            "The latest happenings in the sync."));

        AboutNavItem.Content = Lang.T("Om", "About");

        // The gear is NavigationView's own entry. Its label comes from the framework,
        // that is from Windows' language, so it is set here. It exists only once the
        // control has built its pane.
        if (Navigation.SettingsItem is NavigationViewItem settings)
        {
            settings.Content = Lang.T("Inställningar", "Settings");
            ToolTipService.SetToolTip(settings, settings.Content);
        }

        ShortcutBar.Title = Lang.T("Ingen genväg skapad", "No shortcut created");
        ClearNoticesButton.Content = Lang.T("Rensa meddelanden", "Clear messages");
    }

    /// <summary>
    /// The language change: the labels are rewritten and the frame is replaced, so
    /// the open page is built anew in the new language. A new frame carries nothing
    /// with it, which is what makes the rebuild complete.
    /// </summary>
    private void OnLanguageChanged(object? sender, EventArgs args)
    {
        ApplyLanguage();

        // The held-delete bars carry their language in their buttons: built anew.
        HeldDeletesPanel.Children.Clear();
        _heldBars.Clear();
        UpdateEngineView();
        RefreshUpdateToast();

        var page = _frame.CurrentSourcePageType;
        var slot = ContentHost.Children.IndexOf(_frame);

        // The frame stands in the page row, under the bars.
        var fresh = new Frame();
        Grid.SetRow(fresh, 1);
        if (slot >= 0)
        {
            ContentHost.Children.RemoveAt(slot);
            ContentHost.Children.Insert(slot, fresh);
        }
        else
        {
            ContentHost.Children.Add(fresh);
        }

        _frame = fresh;
        if (page is not null)
        {
            _frame.Navigate(page);
        }
    }

    // ------------------------------------------------------------------ theme

    private const string ThemeKey = "AppTheme";

    /// <summary>The chosen appearance, where Default means "follow Windows".</summary>
    public static ElementTheme CurrentTheme =>
        Instance?.RootGrid.RequestedTheme ?? ElementTheme.Default;

    /// <summary>The user's choice: save it and let it take effect at once.</summary>
    public static void ChangeTheme(ElementTheme theme)
    {
        SaveTheme(theme);
        Instance?.ApplyTheme(theme);
    }

    /// <summary>
    /// RequestedTheme on the root is enough for everything that is XAML, but the
    /// caption buttons are drawn by the system and must be pointed out separately.
    /// Default follows Windows' app mode; an explicit choice locks Light or Dark.
    /// In high contrast the system decides, whatever is chosen.
    /// </summary>
    private void ApplyTheme(ElementTheme theme)
    {
        RootGrid.RequestedTheme = theme;

        AppWindow.TitleBar.PreferredTheme = (theme, IsHighContrast()) switch
        {
            (_, true) => TitleBarTheme.UseDefaultAppMode,
            (ElementTheme.Light, _) => TitleBarTheme.Light,
            (ElementTheme.Dark, _) => TitleBarTheme.Dark,
            _ => TitleBarTheme.UseDefaultAppMode,
        };
    }

    private static ElementTheme LoadTheme()
    {
        try
        {
            return ApplicationData.Current.LocalSettings.Values[ThemeKey] is string saved
                && Enum.TryParse(saved, out ElementTheme theme)
                    ? theme
                    : ElementTheme.Default;
        }
        catch (Exception)
        {
            return ElementTheme.Default;
        }
    }

    private static void SaveTheme(ElementTheme theme)
    {
        try
        {
            ApplicationData.Current.LocalSettings.Values[ThemeKey] = theme.ToString();
        }
        catch (Exception)
        {
            // A choice that could not be saved still applies until the window closes.
        }
    }

    // SPI_GETHIGHCONTRAST, as in Koordinatverktyget: WinRT's AccessibilitySettings
    // needs a CoreWindow and does not exist in a desktop app.
    private const uint SpiGetHighContrast = 0x0042;

    private const uint HighContrastOn = 0x0001;

    [StructLayout(LayoutKind.Sequential)]
    private struct HighContrastInfo
    {
        public uint Size;
        public uint Flags;
        public IntPtr SchemeName;
    }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SystemParametersInfoW(
        uint action, uint parameter, ref HighContrastInfo info, uint update);

    private static bool IsHighContrast()
    {
        try
        {
            var info = new HighContrastInfo { Size = (uint)Marshal.SizeOf<HighContrastInfo>() };
            return SystemParametersInfoW(SpiGetHighContrast, info.Size, ref info, 0)
                && (info.Flags & HighContrastOn) != 0;
        }
        catch (Exception)
        {
            return false;
        }
    }

    // ----------------------------------------------------------------- sizing

    /// <summary>The smallest window the pages are laid out for, in DIPs.</summary>
    private const int MinimumWidthDips = 640;

    private const int MinimumHeightDips = 480;

    private double _minimumSizeScale;

    private void OnRootLoaded(object sender, RoutedEventArgs args)
    {
        ApplyMinimumSize();
        if (RootGrid.XamlRoot is { } root)
        {
            // Fires for every size change too; only a changed scale (another monitor) is acted on.
            root.Changed += (_, _) => ApplyMinimumSize();
        }
    }

    /// <summary>The presenter's limits are in physical pixels, so the DIP figures are scaled first.</summary>
    private void ApplyMinimumSize()
    {
        if (AppWindow.Presenter is not OverlappedPresenter presenter)
        {
            return;
        }

        var scale = RootGrid.XamlRoot?.RasterizationScale ?? 1.0;
        if (scale == _minimumSizeScale)
        {
            return;
        }
        _minimumSizeScale = scale;

        presenter.PreferredMinimumWidth = (int)Math.Round(MinimumWidthDips * scale);
        presenter.PreferredMinimumHeight = (int)Math.Round(MinimumHeightDips * scale);
    }

    // ------------------------------------------------------------- navigation

    /// <summary>
    /// Shows a page by its tag, the way a click in the navigation would: the
    /// selection moves with it, so the pane never points at another page than the
    /// one shown. Used by the pages' own links (Visa aktivitet, Välj projekt).
    /// </summary>
    public void NavigateTo(string tag)
    {
        if (tag == SettingsTag)
        {
            if (Navigation.SettingsItem is null)
            {
                Go(typeof(SettingsPage));
            }
            else if (ReferenceEquals(Navigation.SelectedItem, Navigation.SettingsItem))
            {
                Go(typeof(SettingsPage));
            }
            else
            {
                // Raises SelectionChanged, which navigates (IsSettingsSelected).
                Navigation.SelectedItem = Navigation.SettingsItem;
            }
            return;
        }

        NavigationViewItem? item = tag switch
        {
            OverviewTag => OverviewNavItem,
            ProjectsTag => ProjectsNavItem,
            ActivityTag => ActivityNavItem,
            AboutTag => AboutNavItem,
            _ => null,
        };

        if (item is null)
        {
            return;
        }

        if (ReferenceEquals(Navigation.SelectedItem, item))
        {
            Go(PageFor(tag));
        }
        else
        {
            // Raises SelectionChanged, which navigates.
            Navigation.SelectedItem = item;
        }
    }

    /// <summary>The hamburger lives in the title bar, as in Windows' own Settings.</summary>
    private void OnPaneToggleRequested(TitleBar sender, object args)
    {
        Navigation.IsPaneOpen = !Navigation.IsPaneOpen;
    }

    private void OnNavigationSelectionChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        if (args.IsSettingsSelected)
        {
            Go(typeof(SettingsPage));
            return;
        }

        if (args.SelectedItem is NavigationViewItem { Tag: string tag } && PageFor(tag) is { } page)
        {
            Go(page);
        }
    }

    private static Type? PageFor(string tag) => tag switch
    {
        OverviewTag => typeof(OverviewPage),
        ProjectsTag => typeof(ProjectsPage),
        ActivityTag => typeof(ActivityPage),
        AboutTag => typeof(AboutPage),
        _ => null,
    };

    /// <summary>A navigation to the page already shown adds nothing and is skipped.</summary>
    private void Go(Type? page)
    {
        if (page is not null && _frame.CurrentSourcePageType != page)
        {
            _frame.Navigate(page);
        }
    }
}
