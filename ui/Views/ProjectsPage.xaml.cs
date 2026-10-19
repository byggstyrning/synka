using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Globalization;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using CommunityToolkit.WinUI.Controls;
using Synka.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;

namespace Synka.Views;

public sealed partial class ProjectsPage : Page
{
    /// <summary>How long a switch waits for the engine's `projects` before it shows the engine's state again.</summary>
    private static readonly TimeSpan PendingTimeout = TimeSpan.FromSeconds(20);

    /// <summary>How long a refresh is shown as loading while the engine has not answered with `projects`.</summary>
    private static readonly TimeSpan RefreshAnswerTimeout = TimeSpan.FromSeconds(10);

    /// <summary>The filter waits for a pause in the typing, so a long list is not rebuilt per key.</summary>
    private static readonly TimeSpan FilterDelay = TimeSpan.FromMilliseconds(150);

    /// <summary>A switch the user has flipped and the engine has not yet confirmed. Token tells requests apart.</summary>
    private sealed record PendingToggle(bool Wanted, int Token);

    /// <summary>The controls of one project row, kept so the row can be updated in place.</summary>
    private sealed class ProjectCard
    {
        public ProjectCard(string key, string hubId, string projectId, SettingsCard card, ToggleSwitch toggle, HyperlinkButton open, MenuFlyoutItem openInMenu, HyperlinkButton freeUp, ProgressRing freeUpBusy, Button more, ProgressRing busy)
        {
            Key = key;
            HubId = hubId;
            ProjectId = projectId;
            Card = card;
            Toggle = toggle;
            Open = open;
            OpenInMenu = openInMenu;
            FreeUp = freeUp;
            FreeUpBusy = freeUpBusy;
            More = more;
            Busy = busy;
        }

        public string Key { get; }

        public string HubId { get; }

        public string ProjectId { get; }

        public SettingsCard Card { get; }

        public ToggleSwitch Toggle { get; }

        public HyperlinkButton Open { get; }

        public MenuFlyoutItem OpenInMenu { get; }

        public HyperlinkButton FreeUp { get; }

        public ProgressRing FreeUpBusy { get; }

        public Button More { get; }

        public ProgressRing Busy { get; }

        /// <summary>The project's name, for the reset dialog and the accessible names.</summary>
        public string Name { get; set; } = string.Empty;

        public string? LocalPath { get; set; }
    }

    // Toggles the user has flipped and the engine has not yet confirmed, keyed by
    // "hubId/projectId". The value is the state the user asked for.
    private readonly Dictionary<string, PendingToggle> _pending = new();

    // Hub expansion the user chose, keyed by hub id. Kept across rebuilds.
    private readonly Dictionary<string, bool> _expanded = new();

    private readonly Dictionary<string, ProjectCard> _cards = new();

    private readonly Dictionary<string, SettingsExpander> _hubExpanders = new();

    /// <summary>Which hubs and projects HubsPanel shows now, as one string. A change means a rebuild.</summary>
    private string? _layout;

    /// <summary>True when the hubs on screen were built under a filter (and so were all forced open).</summary>
    private bool _layoutFiltered;

    private string _filter = string.Empty;

    /// <summary>True while the page writes the engine's state into the switches, so Toggled is not taken for the user.</summary>
    private bool _mirroring;

    private int _nextToken;

    /// <summary>One project free-up may run at a time. The key survives a filtered row rebuild.</summary>
    private string? _freeingProjectKey;

    /// <summary>The Open link moves into overflow when the page is too narrow.</summary>
    private bool _compactActions;

    /// <summary>A refresh was asked for and the engine has not yet answered with `projects`.</summary>
    private bool _awaitingProjects;

    /// <summary>The automatic refresh of an empty list is asked for once per visit.</summary>
    private bool _autoRefreshDone;

    private bool _attached;

    private DispatcherQueueTimer? _filterTimer;

    public ProjectsPage()
    {
        InitializeComponent();
    }

    private static EngineClient Engine => EngineClient.Instance;

    private static string Key(string hubId, string projectId) => hubId + "/" + projectId;

    // ------------------------------------------------------------ lifetime

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        if (_attached)
        {
            return;
        }
        _attached = true;

        _compactActions = ActualWidth > 0 && ActualWidth < 720;
        if (!Engine.IsFreeingUp)
        {
            _freeingProjectKey = null;
        }

        Engine.PropertyChanged += OnEnginePropertyChanged;
        Update();
        RefreshIfEmpty();
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        Engine.PropertyChanged -= OnEnginePropertyChanged;
        if (_filterTimer is not null)
        {
            _filterTimer.Stop();
            _filterTimer.Tick -= OnFilterTick;
            _filterTimer = null;
        }
    }

    private void OnEnginePropertyChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(EngineClient.Projects):
                _awaitingProjects = false;
                Update();
                RefreshIfEmpty();
                break;

            // Status comes up to twice a second; only the sign-in in it matters here,
            // and that is raised on its own.
            case nameof(EngineClient.IsSignedIn):
            case nameof(EngineClient.IsConnected):
                Update();
                RefreshIfEmpty();
                break;

            case nameof(EngineClient.IsFreeingUp):
                if (!Engine.IsFreeingUp && _freeingProjectKey is not null)
                {
                    FinishFreeUp();
                }
                else
                {
                    Update();
                }
                break;
        }
    }

    private void OnPageSizeChanged(object sender, SizeChangedEventArgs e)
    {
        var compact = e.NewSize.Width < 720;
        if (_compactActions != compact)
        {
            // If the visible Open link is about to move into overflow, leave
            // keyboard users on the project's More button rather than nowhere.
            string? openKey = null;
            if (compact && XamlRoot is { } root &&
                FocusManager.GetFocusedElement(root) is HyperlinkButton { Tag: string tag } &&
                tag.StartsWith("o|", StringComparison.Ordinal))
            {
                openKey = tag[2..];
            }

            _compactActions = compact;
            if (_attached)
            {
                Update();
                if (openKey is not null && _cards.TryGetValue(openKey, out var card))
                {
                    card.More.Focus(FocusState.Programmatic);
                }
            }
        }
    }

    /// <summary>
    /// The engine starts with an empty listing (docs\IPC.md, `hello`). The first
    /// visit while signed in asks for the hubs, once.
    /// </summary>
    private void RefreshIfEmpty()
    {
        if (_autoRefreshDone || !Engine.IsSignedIn)
        {
            return;
        }

        var projects = Engine.Projects;
        if (projects is { Loading: true } || (projects?.Hubs?.Count ?? 0) > 0)
        {
            return;
        }

        _autoRefreshDone = true;
        _ = RefreshAsync();
    }

    // ------------------------------------------------------------ commands

    private void OnRefresh(object sender, RoutedEventArgs e)
    {
        _ = RefreshAsync();
    }

    private async Task RefreshAsync()
    {
        _awaitingProjects = true;
        ErrorBar.IsOpen = false;
        Update();

        var ack = await Engine.RefreshProjectsAsync();
        if (!ack.Ok)
        {
            _awaitingProjects = false;
            ShowError(ack.Error, Lang.T("Projekten gick inte att hämta.", "The projects could not be fetched."));
            Update();
            return;
        }

        // The answer is a `projects` event. If none comes, the spinner must not run for ever.
        await Task.Delay(RefreshAnswerTimeout);
        if (_awaitingProjects)
        {
            _awaitingProjects = false;
            Update();
        }
    }

    private async void OnSignIn(object sender, RoutedEventArgs e)
    {
        var ack = await Engine.SignInAsync();
        if (!ack.Ok)
        {
            ShowError(ack.Error, Lang.T("Inloggningen gick inte att starta.", "Sign-in could not be started."));
        }
        else
        {
            EmptyText.Text = Lang.T(
                "Slutför inloggningen i webbläsaren. Projekten visas här sedan.",
                "Finish signing in in the browser. The projects then show here.");
        }
    }

    private void OnToggled(object sender, RoutedEventArgs e)
    {
        if (_mirroring
            || Engine.IsFreeingUp
            || sender is not ToggleSwitch { Tag: string tag } toggle
            || tag.Length < 2
            || !_cards.TryGetValue(tag[2..], out var card))
        {
            return;
        }

        _ = SetProjectAsync(card, toggle.IsOn);
    }

    /// <summary>
    /// Sends the switch to the engine. The switch stays where the user put it, with
    /// a ring beside it, until the engine's `projects` confirms it. It is not
    /// disabled meanwhile: a disabled control loses keyboard focus.
    /// </summary>
    private async Task SetProjectAsync(ProjectCard card, bool subscribe)
    {
        var token = ++_nextToken;
        _pending[card.Key] = new PendingToggle(subscribe, token);
        ErrorBar.IsOpen = false;
        Update();

        var ack = await Engine.SetProjectAsync(card.HubId, card.ProjectId, subscribe);
        if (!ack.Ok)
        {
            if (IsCurrent(card.Key, token))
            {
                _pending.Remove(card.Key);
            }

            ShowError(ack.Error, Lang.T("Det gick inte att ändra projektet.", "The project could not be changed."));
            Update();
            return;
        }

        // Accepted. The confirmation is the next `projects` (see ApplyState). If it
        // never comes (an unknown project gives only a notify), the switch goes
        // back to what the engine last said.
        await Task.Delay(PendingTimeout);
        if (IsCurrent(card.Key, token))
        {
            _pending.Remove(card.Key);
            Update();
        }
    }

    private bool IsCurrent(string key, int token) =>
        _pending.TryGetValue(key, out var request) && request.Token == token;

    private async void OnOpenProject(object sender, RoutedEventArgs e)
    {
        var tag = sender switch
        {
            HyperlinkButton { Tag: string value } => value,
            MenuFlyoutItem { Tag: string value } => value,
            _ => null,
        };
        if (tag is null || tag.Length < 2
            || !_cards.TryGetValue(tag[2..], out var card)
            || string.IsNullOrEmpty(card.LocalPath))
        {
            return;
        }

        var ack = await Engine.OpenFolderAsync(card.LocalPath);
        if (!ack.Ok)
        {
            ShowError(ack.Error, Lang.T("Mappen gick inte att öppna.", "The folder could not be opened."));
        }
    }

    /// <summary>
    /// Frees clean local copies in this project. The ack only accepts the job;
    /// the engine reports completion through a notification and activity item.
    /// </summary>
    private async void OnFreeUpProject(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement { Tag: string tag }
            || tag.Length < 2
            || !_cards.TryGetValue(tag[2..], out var card)
            || _freeingProjectKey is not null
            || Engine.IsFreeingUp
            || !Engine.IsSignedIn
            || _pending.ContainsKey(card.Key)
            || Engine.Projects?.Hubs?.Any(h => h.Id == card.HubId &&
                h.Projects?.Any(p => p.Id == card.ProjectId && p.Subscribed) == true) != true)
        {
            return;
        }

        _freeingProjectKey = card.Key;
        ErrorBar.IsOpen = false;
        Update();

        var ack = await Engine.FreeUpSpaceAsync(card.ProjectId);
        if (!ack.Ok)
        {
            FinishFreeUp(card.Key);
            ShowError(ack.Error, Lang.T("Utrymmet gick inte att frigöra.", "The space could not be freed."));
            return;
        }

        // The shared client tracks the engine's completion notification and
        // timeout. It may have completed before this command ack arrived.
        if (!Engine.IsFreeingUp)
        {
            FinishFreeUp(card.Key);
        }
    }

    private void FinishFreeUp(string? expectedKey = null)
    {
        if (_freeingProjectKey is null || expectedKey is not null && _freeingProjectKey != expectedKey)
        {
            return;
        }

        _freeingProjectKey = null;
        if (_attached)
        {
            Update();
        }
    }

    /// <summary>
    /// Rebuilds a synced project from ACC, once the user has said yes. The engine saves
    /// unsynced local changes aside first; the result comes as a notify and an
    /// activity item (kind "reset"), so nothing more is shown here on success.
    /// </summary>
    private async void OnResetProject(object sender, RoutedEventArgs e)
    {
        if (sender is not MenuFlyoutItem { Tag: string tag }
            || tag.Length < 2
            || !_cards.TryGetValue(tag[2..], out var card)
            || _freeingProjectKey is not null
            || Engine.IsFreeingUp
            || !Engine.IsSignedIn
            || _pending.ContainsKey(card.Key)
            || Engine.Projects?.Hubs?.Any(h => h.Id == card.HubId &&
                h.Projects?.Any(p => p.Id == card.ProjectId && p.Subscribed) == true) != true
            || XamlRoot is not { } root)
        {
            return;
        }

        var body = new StackPanel { Spacing = 8 };
        body.Children.Add(new TextBlock
        {
            Text = card.Name,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Lang.T(
                "Projektmappen byggs om från ACC. Filer laddas ner igen när du öppnar dem. Lokala ändringar som inte har synkats sparas först i mappen \"Synka - återställda filer\" bredvid synkmappen.",
                "The project folder is rebuilt from ACC. Files are downloaded again when you open them. Local changes that have not been synced are first saved in the folder \"Synka - återställda filer\" next to the sync folder."),
            TextWrapping = TextWrapping.Wrap,
        });

        var dialog = new ContentDialog
        {
            XamlRoot = root,
            // The dialog hangs in the popup root and does not inherit the theme choice.
            RequestedTheme = MainWindow.CurrentTheme,
            Title = Lang.T("Bygg om projektet från ACC?", "Rebuild the project from ACC?"),
            Content = body,
            PrimaryButtonText = Lang.T("Bygg om", "Rebuild"),
            CloseButtonText = Lang.T("Avbryt", "Cancel"),
            // Enter must not rebuild a project by accident.
            DefaultButton = ContentDialogButton.Close,
        };

        ContentDialogResult answer;
        try
        {
            answer = await dialog.ShowAsync();
        }
        catch (Exception)
        {
            // Another dialog is open, or the window is closing. Nothing was asked, nothing is done.
            return;
        }

        if (answer != ContentDialogResult.Primary
            || Engine.IsFreeingUp
            || !Engine.IsSignedIn
            || _pending.ContainsKey(card.Key)
            || Engine.Projects?.Hubs?.Any(h => h.Id == card.HubId &&
                h.Projects?.Any(p => p.Id == card.ProjectId && p.Subscribed) == true) != true)
        {
            return;
        }

        var ack = await Engine.ResetProjectAsync(card.ProjectId);
        if (!ack.Ok)
        {
            ShowError(ack.Error, Lang.T("Projektet gick inte att återställa.", "The project could not be reset."));
        }
    }

    private void ShowError(string? message, string fallback)
    {
        ErrorBar.Message = string.IsNullOrWhiteSpace(message) ? fallback : message;
        ErrorBar.IsOpen = true;
    }

    // -------------------------------------------------------------- filter

    private void OnFilterChanged(object sender, TextChangedEventArgs e)
    {
        if (_filterTimer is null)
        {
            _filterTimer = DispatcherQueue.CreateTimer();
            _filterTimer.IsRepeating = false;
            _filterTimer.Interval = FilterDelay;
            _filterTimer.Tick += OnFilterTick;
        }

        _filterTimer.Stop();
        _filterTimer.Start();
    }

    private void OnFilterTick(DispatcherQueueTimer sender, object args)
    {
        sender.Stop();
        if (_filter != FilterBox.Text)
        {
            _filter = FilterBox.Text;
            Update();
        }
    }

    private void OnFilterKeyDown(object sender, KeyRoutedEventArgs e)
    {
        // Windows.System spelled out: a using of it would make DispatcherQueueTimer ambiguous.
        if (e.Key == Windows.System.VirtualKey.Escape && FilterBox.Text.Length > 0)
        {
            FilterBox.Text = string.Empty;
            e.Handled = true;
        }
    }

    private void OnFindInvoked(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        FilterBox.Focus(FocusState.Keyboard);
        FilterBox.SelectAll();
        args.Handled = true;
    }

    // -------------------------------------------------------------- the list

    /// <summary>
    /// Shows the engine's hub tree under the current filter. When the same hubs and
    /// projects are shown as before, only their state is written into the existing
    /// controls; the list is rebuilt only when what is shown changes.
    /// </summary>
    private void Update()
    {
        var info = Engine.Projects;
        var loading = (info?.Loading ?? false) || _awaitingProjects;
        RefreshSpinner.IsActive = loading;
        RefreshSpinner.Visibility = loading ? Visibility.Visible : Visibility.Collapsed;
        RefreshGlyph.Visibility = loading ? Visibility.Collapsed : Visibility.Visible;
        RefreshButton.IsEnabled = Engine.IsSignedIn;

        var filter = Normalize(_filter);
        var shown = new List<(HubInfo Hub, List<ProjectInfo> All, List<ProjectInfo> Visible)>();
        foreach (var hub in info?.Hubs ?? new List<HubInfo>())
        {
            if (hub?.Id is null)
            {
                continue;
            }

            var all =(hub.Projects ?? new List<ProjectInfo>()).Where(p => p is not null).ToList();

            // A hub that matches shows all its projects; otherwise only the matching ones.
            var hubMatches = filter.Length == 0 || Normalize(hub.Name).Contains(filter, StringComparison.Ordinal);
            var visible = all
                .Where(p => hubMatches || Normalize(p.Name).Contains(filter, StringComparison.Ordinal))
                .ToList();
            if (visible.Count > 0)
            {
                shown.Add((hub, all, visible));
            }
        }

        var layout = string.Join("\n", shown.Select(s => s.Hub.Id + ":" + string.Join(",", s.Visible.Select(p => p.Id))));
        if (layout != _layout)
        {
            BuildLayout(shown, filter.Length > 0);
            _layout = layout;
        }

        _mirroring = true;
        try
        {
            foreach (var (hub, all, visible) in shown)
            {
                if (_hubExpanders.TryGetValue(hub.Id, out var expander))
                {
                    var synced = all.Count(p => p.Subscribed);
                    expander.Header = hub.Name;
                    expander.Description = Lang.T(
                        $"{synced} av {all.Count} projekt synkas",
                        $"{synced} of {all.Count} projects synced");
                }

                foreach (var project in visible)
                {
                    if (_cards.TryGetValue(Key(hub.Id, project.Id), out var card))
                    {
                        ApplyState(card, project);
                    }
                }
            }
        }
        finally
        {
            _mirroring = false;
        }

        ShowEmptyState(shown.Count, filter.Length > 0, loading);
    }

    /// <summary>Creates the hub expanders and project cards anew, keeping expansion and keyboard focus.</summary>
    private void BuildLayout(List<(HubInfo Hub, List<ProjectInfo> All, List<ProjectInfo> Visible)> shown, bool filtered)
    {
        // Keep what the user chose for each hub, but only from an unfiltered view:
        // a filter forces hubs open and must not be remembered as a choice.
        if (!_layoutFiltered)
        {
            foreach (var (hubId, old) in _hubExpanders)
            {
                _expanded[hubId] = old.IsExpanded;
            }
        }

        // The control that has focus, if it is one of ours, gets it back once rebuilt.
        string? focusTag = null;
        var focusState = FocusState.Programmatic;
        if (XamlRoot is { } root && FocusManager.GetFocusedElement(root) is Control { Tag: string tag } focused
            && (tag.StartsWith("t|", StringComparison.Ordinal)
                || tag.StartsWith("o|", StringComparison.Ordinal)
                || tag.StartsWith("f|", StringComparison.Ordinal)
                || tag.StartsWith("m|", StringComparison.Ordinal)))
        {
            focusTag = tag;
            focusState = focused.FocusState == FocusState.Unfocused ? FocusState.Programmatic : focused.FocusState;
        }

        HubsPanel.Children.Clear();
        _cards.Clear();
        _hubExpanders.Clear();

        Control? focusTarget = null;
        foreach (var (hub, all, visible) in shown)
        {
            var expander = new SettingsExpander
            {
                Header = hub.Name,
                HeaderIcon = new FontIcon { Glyph = "" },
                IsExpanded = filtered
                    || (_expanded.TryGetValue(hub.Id, out var remembered) ? remembered : all.Any(p => p.Subscribed)),
                Tag = hub.Id,
            };
            AutomationProperties.SetName(expander, hub.Name);

            foreach (var project in visible)
            {
                var card = BuildProjectCard(hub, project);
                _cards[card.Key] = card;
                expander.Items.Add(card.Card);

                if (focusTag is not null)
                {
                    if (focusTag == (string)card.Toggle.Tag)
                    {
                        focusTarget = card.Toggle;
                    }
                    else if (focusTag == (string)card.Open.Tag)
                    {
                        focusTarget = card.Open;
                    }
                    else if (focusTag == (string)card.FreeUp.Tag)
                    {
                        focusTarget = card.FreeUp;
                    }
                    else if (focusTag == (string)card.More.Tag)
                    {
                        focusTarget = card.More;
                    }
                }
            }

            _hubExpanders[hub.Id] = expander;
            HubsPanel.Children.Add(expander);
        }

        _layoutFiltered = filtered;

        if (focusTarget is not null)
        {
            // The new control can take focus only once it is in the live tree.
            var target = focusTarget;
            void OnTargetLoaded(object s, RoutedEventArgs a)
            {
                target.Loaded -= OnTargetLoaded;
                target.Focus(focusState);
            }

            target.Loaded += OnTargetLoaded;
        }
    }

    private ProjectCard BuildProjectCard(HubInfo hub, ProjectInfo project)
    {
        var key = Key(hub.Id, project.Id);

        var toggle = new ToggleSwitch
        {
            OnContent = Lang.T("Synkas", "Synced"),
            OffContent = Lang.T("Synkas inte", "Not synced"),
            Tag = "t|" + key,
        };
        AutomationProperties.SetName(toggle, Lang.T($"Synka {project.Name}", $"Sync {project.Name}"));
        toggle.Toggled += OnToggled;

        var open = new HyperlinkButton
        {
            Content = Lang.T("Öppna", "Open"),
            Tag = "o|" + key,
            Visibility = Visibility.Collapsed,
        };
        AutomationProperties.SetName(open, Lang.T($"Öppna mappen för {project.Name}", $"Open the folder of {project.Name}"));
        ToolTipService.SetToolTip(open, Lang.T("Öppna projektmappen i Utforskaren", "Open the project folder in File Explorer"));
        open.Click += OnOpenProject;

        var freeUpSpinner = new ProgressRing
        {
            Width = 14,
            Height = 14,
            IsActive = false,
            Visibility = Visibility.Collapsed,
        };
        var freeUpSlot = new Grid { Width = 14, Height = 14 };
        freeUpSlot.Children.Add(freeUpSpinner);
        var freeUpContent = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 6,
        };
        freeUpContent.Children.Add(new TextBlock { Text = Lang.T("Frigör utrymme", "Free up space") });
        freeUpContent.Children.Add(freeUpSlot);

        var freeUp = new HyperlinkButton
        {
            Content = freeUpContent,
            Tag = "f|" + key,
            Visibility = Visibility.Collapsed,
        };
        AutomationProperties.SetName(freeUp, Lang.T($"Frigör utrymme i {project.Name}", $"Free up space in {project.Name}"));
        ToolTipService.SetToolTip(freeUp, Lang.T(
            "Ta bort synkade lokala kopior och avmarkera Behåll alltid på den här enheten i projektet. Osynkade ändringar behålls. En fäst hubbmapp kan hålla filer lokalt; använd Frigör i alla projekt i Inställningar för att avmarkera den.",
            "Remove synced local copies and clear Always keep on this device in this project. Unsynced changes stay local. A pinned hub folder can keep files local; use Free up in all projects in Settings to clear that pin."));
        freeUp.Click += OnFreeUpProject;

        // Keep the targeted rebuild available without making the common row too wide.
        var openInMenu = new MenuFlyoutItem
        {
            Text = Lang.T("Öppna mappen", "Open folder"),
            Tag = "o|" + key,
            Visibility = Visibility.Collapsed,
        };
        AutomationProperties.SetName(openInMenu, Lang.T($"Öppna mappen för {project.Name}", $"Open the folder of {project.Name}"));
        openInMenu.Click += OnOpenProject;

        var reset = new MenuFlyoutItem
        {
            Text = Lang.T("Bygg om från ACC …", "Rebuild from ACC …"),
            Tag = "r|" + key,
        };
        AutomationProperties.SetName(reset, Lang.T($"Återställ {project.Name} från ACC", $"Reset {project.Name} from ACC"));
        reset.Click += OnResetProject;
        var menu = new MenuFlyout();
        menu.Items.Add(openInMenu);
        menu.Items.Add(reset);
        var more = new Button
        {
            Content = new FontIcon { Glyph = "\uE712", FontSize = 16 },
            Flyout = menu,
            Tag = "m|" + key,
            MinWidth = 32,
            MinHeight = 32,
            Padding = new Thickness(6, 0, 6, 0),
            Visibility = Visibility.Collapsed,
        };
        AutomationProperties.SetName(more, Lang.T($"Fler åtgärder för {project.Name}", $"More actions for {project.Name}"));
        ToolTipService.SetToolTip(more, Lang.T("Fler projektåtgärder", "More project actions"));

        var busy = new ProgressRing
        {
            Width = 16,
            Height = 16,
            IsActive = false,
            Visibility = Visibility.Collapsed,
        };
        AutomationProperties.SetName(busy, Lang.T("Väntar på motorn", "Waiting for the engine"));

        var actions = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 6,
            VerticalAlignment = VerticalAlignment.Center,
        };
        actions.Children.Add(freeUp);
        actions.Children.Add(open);
        actions.Children.Add(more);
        actions.Children.Add(busy);
        actions.Children.Add(toggle);

        var card = new SettingsCard
        {
            Header = project.Name,
            Content = actions,
        };

        return new ProjectCard(key, hub.Id, project.Id, card, toggle, open, openInMenu, freeUp, freeUpSpinner, more, busy) { Name = project.Name ?? string.Empty };
    }

    /// <summary>Writes the engine's state for one project (and any pending request) into its card.</summary>
    private void ApplyState(ProjectCard card, ProjectInfo project)
    {
        var isOn = project.Subscribed;
        var isPending = false;

        if (_pending.TryGetValue(card.Key, out var request))
        {
            if (project.Subscribed == request.Wanted)
            {
                // The engine has caught up with the request: nothing is pending any more.
                _pending.Remove(card.Key);
            }
            else
            {
                isOn = request.Wanted;
                isPending = true;
            }
        }

        if (card.Toggle.IsOn != isOn)
        {
            card.Toggle.IsOn = isOn;
        }

        card.Busy.IsActive = isPending;
        card.Busy.Visibility = isPending ? Visibility.Visible : Visibility.Collapsed;

        var path = project.Subscribed && !isPending ? project.LocalPath : null;
        card.LocalPath = path;
        var canOpen = !string.IsNullOrEmpty(path);
        card.Open.Visibility = canOpen && !_compactActions ? Visibility.Visible : Visibility.Collapsed;
        card.OpenInMenu.Visibility = canOpen && _compactActions ? Visibility.Visible : Visibility.Collapsed;
        var showActions = project.Subscribed && !isPending;
        card.FreeUp.Visibility = showActions ? Visibility.Visible : Visibility.Collapsed;
        card.More.Visibility = showActions ? Visibility.Visible : Visibility.Collapsed;
        card.FreeUp.IsEnabled = Engine.IsSignedIn && !Engine.IsFreeingUp && _freeingProjectKey is null;
        card.More.IsEnabled = Engine.IsSignedIn && !Engine.IsFreeingUp && _freeingProjectKey is null;
        var freeingThisProject = _freeingProjectKey == card.Key;
        // Unsubscribing removes the project's clean directory and DB rows. Do not
        // let it run while free-up is scanning the tree.
        card.Toggle.IsEnabled = Engine.IsSignedIn && !Engine.IsFreeingUp && !freeingThisProject;
        card.FreeUpBusy.IsActive = freeingThisProject;
        card.FreeUpBusy.Visibility = freeingThisProject ? Visibility.Visible : Visibility.Collapsed;

        card.Name = project.Name ?? string.Empty;
        card.Card.Header = card.Name;
        card.Card.Description = isPending
            ? (isOn ? Lang.T("Börjar synka …", "Starting to sync …") : Lang.T("Slutar synka …", "Stopping the sync …"))
            : path ?? string.Empty;
    }

    private void ShowEmptyState(int shownCount, bool filtered, bool loading)
    {
        if (shownCount > 0)
        {
            EmptyPanel.Visibility = Visibility.Collapsed;
            EmptySpinner.IsActive = false;
            return;
        }

        var spin = false;
        var signIn = false;
        string text;
        if (!Engine.IsConnected)
        {
            text = Lang.T(
                "Projekten visas när fönstret har kontakt med motorn.",
                "The projects show once the window is in contact with the engine.");
        }
        else if (!Engine.IsSignedIn)
        {
            text = Lang.T("Logga in för att se dina ACC-projekt.", "Sign in to see your ACC projects.");
            signIn = true;
        }
        else if (filtered)
        {
            text = Lang.T("Inget matchar filtret.", "Nothing matches the filter.");
        }
        else if (loading)
        {
            text = Lang.T("Hämtar projekt från ACC …", "Fetching projects from ACC …");
            spin = true;
        }
        else
        {
            text = Lang.T(
                $"Inga projekt hittades. {AppInfo.Name} måste vara tillagt som Custom Integration i ACC-kontot. Tryck sedan på Uppdatera.",
                $"No projects found. {AppInfo.Name} has to be added as a Custom Integration in the ACC account. Then press Refresh.");
        }

        EmptyText.Text = text;
        EmptySpinner.IsActive = spin;
        EmptySpinner.Visibility = spin ? Visibility.Visible : Visibility.Collapsed;
        EmptySignInButton.Visibility = signIn ? Visibility.Visible : Visibility.Collapsed;
        EmptyPanel.Visibility = Visibility.Visible;
    }

    /// <summary>
    /// Lowercases and strips diacritics and combining marks, so "Köpmannen"
    /// becomes "kopmannen" and matches the same way the user types it.
    /// </summary>
    private static string Normalize(string? text)
    {
        if (string.IsNullOrWhiteSpace(text))
        {
            return string.Empty;
        }

        var decomposed = text.Trim().Normalize(NormalizationForm.FormD);
        var sb = new StringBuilder(decomposed.Length);
        foreach (var c in decomposed)
        {
            if (CharUnicodeInfo.GetUnicodeCategory(c) != UnicodeCategory.NonSpacingMark)
            {
                sb.Append(c);
            }
        }

        return sb.ToString().ToLowerInvariant();
    }
}
