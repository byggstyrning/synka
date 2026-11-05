using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Linq;
using System.Threading.Tasks;
using CommunityToolkit.WinUI.Controls;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace Synka.Views;

/// <summary>
/// Konton: the accounts of every platform Synka syncs from, one section each (ACC, StreamBIM,
/// Interaxo). Every signed-in or signing-in account has a card that is always shown: its server,
/// its state, its projects, and sign-out. The sign-in dialogs are in AccountsPage.SignIn.cs, the
/// advanced client settings in AccountsPage.Advanced.cs.
/// </summary>
public sealed partial class AccountsPage : Page
{
    private readonly EngineClient _engine = EngineClient.Instance;

    /// <summary>The card of each account on screen, by <see cref="CardKey"/>. A refresh updates them in place, so focus stays put.</summary>
    private readonly Dictionary<string, AccountCard> _cards = new();

    private bool _attached;

    /// <summary>True while a ContentDialog of this page is open. WinUI shows one at a time.</summary>
    private bool _dialogOpen;

    /// <summary>The StreamBIM sign-in dialog while it is open. It takes the engine's progress for its account.</summary>
    private StreamBimSignIn? _streamBimSignIn;

    /// <summary>The browser sign-in dialog (ACC or Interaxo) while it is open.</summary>
    private BrowserSignIn? _browserSignIn;

    /// <summary>The last StreamBIM account whose sign-in failed. Its failure shows in the StreamBIM section until closed.</summary>
    private string? _streamBimFailureKey;

    /// <summary>The failures the user has closed or retried. The same failure does not come back; a new one does. Keyed by <see cref="FailureKey"/>.</summary>
    private readonly Dictionary<string, SignInProgressInfo> _dismissedFailures = new();

    /// <summary>The code prompts the user closed with Avbryt, per StreamBIM account. Not shown again while the engine still reports that same prompt.</summary>
    private readonly Dictionary<string, SignInProgressInfo> _dismissedCodePrompts = new();

    /// <summary>True while the page writes the engine's Interaxo settings into the boxes, so TextChanged is not taken for the user.</summary>
    private bool _mirroring;

    /// <summary>The user has typed in the Interaxo boxes since the last save: the engine's values then wait.</summary>
    private bool _interaxoEdited;

    public AccountsPage()
    {
        InitializeComponent();
    }

    private static string NoContactText => Lang.T("Ingen kontakt med motorn.", "No contact with the engine.");

    // ------------------------------------------------------------------ lifetime

    private void OnLoaded(object sender, RoutedEventArgs args)
    {
        if (_attached)
        {
            return;
        }
        _attached = true;

        _engine.PropertyChanged += OnEnginePropertyChanged;
        _engine.SignInProgress += OnSignInProgress;
        _engine.StreamBimRegionsChanged += OnStreamBimRegionsChanged;
        _engine.Notified += OnApsClientIdNotified;

        RefreshSettings();
        RefreshAll();
        RequestMissing();

        // The engine may be waiting for a StreamBIM code that was asked for while this page was closed.
        ShowCodePromptIfWanted();
    }

    private void OnUnloaded(object sender, RoutedEventArgs args)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        _engine.PropertyChanged -= OnEnginePropertyChanged;
        _engine.SignInProgress -= OnSignInProgress;
        _engine.StreamBimRegionsChanged -= OnStreamBimRegionsChanged;
        _engine.Notified -= OnApsClientIdNotified;
    }

    // -------------------------------------------------------------- engine events

    /// <summary>Status comes up to twice a second: each event refreshes the page, which writes only what changed.</summary>
    private void OnEnginePropertyChanged(object? sender, PropertyChangedEventArgs args)
    {
        switch (args.PropertyName)
        {
            case nameof(EngineClient.Status):
            case nameof(EngineClient.Projects):
            case nameof(EngineClient.IsSignedIn):
            case nameof(EngineClient.IsFreeingUp):
                RefreshAll();
                break;

            case nameof(EngineClient.ApsClientIdValidation):
                RefreshApsSettings(_engine.Settings);
                RefreshAll();
                break;

            case nameof(EngineClient.Settings):
                RefreshSettings();
                RefreshAll();
                break;

            case nameof(EngineClient.IsConnected):
                // The engine's StreamBIM code wait does not outlive the connection: its dialog closes with it.
                if (!_engine.IsConnected)
                {
                    _streamBimSignIn?.CloseForDisconnect();
                }
                RefreshAll();
                RequestMissing();
                break;
        }
    }

    private void OnStreamBimRegionsChanged(StreamBimRegionsInfo regions)
    {
        _streamBimSignIn?.RegionsChanged();
        RefreshAll();
    }

    /// <summary>
    /// The engine's sign-in messages. ACC, Interaxo and StreamBIM each have one dialog at a time;
    /// the dialog that waits for the account takes the message, the page shows the rest.
    /// </summary>
    private void OnSignInProgress(SignInProgressInfo progress)
    {
        if (progress.Provider == ProviderNames.StreamBim)
        {
            if (progress.State == "failed" && !string.IsNullOrEmpty(progress.Account))
            {
                _streamBimFailureKey = progress.Account;
            }
            _streamBimSignIn?.OnProgress(progress);
        }
        else if (_browserSignIn is { } browser && ProviderNames.Id(progress.Provider) == browser.Provider)
        {
            browser.OnProgress(progress);
        }

        RefreshAll();
        ShowCodePromptIfWanted();
    }

    /// <summary>Asks the engine for what the page needs and does not have: the projects and the StreamBIM regions.</summary>
    private void RequestMissing()
    {
        if (!_engine.IsConnected)
        {
            return;
        }

        if (_engine.StreamBimRegions is null)
        {
            _ = RequestRegionsAsync();
        }

        var projects = _engine.Projects;
        if (_engine.IsSignedIn && !(projects?.Loading ?? false) && (projects?.Hubs?.Count ?? 0) == 0)
        {
            _ = RefreshProjectsAsync();
        }
    }

    private async Task RequestRegionsAsync()
    {
        ReportIfFailed(await _engine.RequestStreamBimRegionsAsync(),
            Lang.T("Regionerna gick inte att hämta", "The regions could not be fetched"));
    }

    private async Task RefreshProjectsAsync()
    {
        ReportIfFailed(await _engine.RefreshProjectsAsync(),
            Lang.T("Projekten gick inte att hämta", "The projects could not be fetched"));
    }

    // ------------------------------------------------------------------ refresh

    /// <summary>Writes the engine's state into every part of the page.</summary>
    private void RefreshAll()
    {
        if (!_attached)
        {
            return;
        }

        var connected = _engine.IsConnected;
        RefreshSummary(connected);
        RefreshAccSection(connected);
        RefreshStreamBimSection(connected);
        RefreshInteraxoSection(connected);
        RefreshApsControls();
        _streamBimSignIn?.Refresh();
    }

    /// <summary>"2 konton inloggade · 3 projekt synkas": the signed-in accounts and their subscribed projects.</summary>
    private void RefreshSummary(bool connected)
    {
        if (!connected)
        {
            SetText(SummaryText, NoContactText);
            return;
        }

        // Only the providers the engine offers count: a provider it leaves out (Interaxo, for now) is not shown.
        var signedIn = ReportedAccounts()
            .Where(account => account.SignedIn && _engine.ProviderOffered(ProviderNames.Id(account.Provider)))
            .ToList();
        var accounts = signedIn.Count == 1
            ? Lang.T("1 konto inloggat", "1 account signed in")
            : Lang.T($"{signedIn.Count} konton inloggade", $"{signedIn.Count} accounts signed in");
        var synced = signedIn.Sum(account => SubscribedCount(account));
        var projects = synced == 1
            ? Lang.T("1 projekt synkas", "1 project synced")
            : Lang.T($"{synced} projekt synkas", $"{synced} projects synced");
        SetText(SummaryText, $"{accounts} · {projects}");
    }

    private void RefreshAccSection(bool connected)
    {
        var account = connected ? _engine.AccountFor(ProviderNames.Acc) : null;
        var shown = Shown(account);
        ShowCards(AccCards, ProviderNames.Acc, shown);

        var signedIn = account is { SignedIn: true };
        Show(AccSignInButton, !signedIn);
        AccSignInButton.IsEnabled = connected && account is { Configured: true, SignedIn: false } && !ApsEdit.IsWaiting;
        RefreshEmpty(AccEmptyPanel, AccEmptyBadge, AccEmptyText, shown.Count == 0, connected, older: false);
        ShowFailure(AccError, ProviderNames.Acc);
    }

    private void RefreshStreamBimSection(bool connected)
    {
        // The ACC-only build has no StreamBIM: no section at all.
        Show(StreamBimSection, _engine.ProviderInBuild(ProviderNames.StreamBim));

        var shown = connected
            ? _engine.StreamBimAccounts
                .Where(account => (account.SignedIn || account.SigningIn) && !string.IsNullOrEmpty(account.Account))
                .ToList()
            : new List<AccountInfo>();
        ShowCards(StreamBimCards, ProviderNames.StreamBim, shown);

        // An engine without `accounts` (older) takes no StreamBIM sign-in: the button waits for an update.
        var older = connected && _engine.Status is { Accounts: null };
        StreamBimAddButton.IsEnabled = connected && _engine.Status is not null && !older;
        RefreshEmpty(StreamBimEmptyPanel, StreamBimEmptyBadge, StreamBimEmptyText, shown.Count == 0, connected, older);
        ShowFailure(StreamBimError, ProviderNames.StreamBim, _streamBimFailureKey);
    }

    /// <summary>
    /// Interaxo while the engine leaves it out of settings.providers (turned off for now): the name and one
    /// muted line. No sign-in, no account card, no client settings, and no failure bar. The ACC-only
    /// build has no Interaxo: no section at all.
    /// </summary>
    private void RefreshInteraxoSection(bool connected)
    {
        Show(InteraxoSection, _engine.ProviderInBuild(ProviderNames.Interaxo));

        var offered = _engine.ProviderOffered(ProviderNames.Interaxo);
        Show(InteraxoCaption, offered);
        Show(InteraxoOffText, !offered);
        Show(InteraxoHeaderActions, offered);
        Show(InteraxoAdvanced, offered);
        Show(InteraxoError, offered);

        var account = connected && offered ? _engine.AccountFor(ProviderNames.Interaxo) : null;
        var shown = Shown(account);
        ShowCards(InteraxoCards, ProviderNames.Interaxo, shown);
        if (!offered)
        {
            return;
        }

        var signedIn = account is { SignedIn: true };
        Show(InteraxoSignInButton, !signedIn);
        InteraxoSignInButton.IsEnabled = connected && account is { SignedIn: false, Configured: true };
        Show(InteraxoHint, connected && account is { SignedIn: false, Configured: false });
        var older = connected && _engine.Status is { Accounts: null };
        RefreshEmpty(InteraxoEmptyPanel, InteraxoEmptyBadge, InteraxoEmptyText, shown.Count == 0, connected, older);
        ShowFailure(InteraxoError, ProviderNames.Interaxo);

        // The client settings change only while Interaxo is signed out (docs\IPC.md).
        var editable = connected && account is { SignedIn: false, SigningIn: false };
        InteraxoClientIdBox.IsEnabled = editable;
        InteraxoRedirectBox.IsEnabled = editable;
        InteraxoSecretBox.IsEnabled = editable;
        InteraxoSaveButton.IsEnabled = editable;
        InteraxoClearSecretButton.IsEnabled = editable;
    }

    /// <summary>
    /// A section with no account on screen says so: "Utloggad" and "Inte inloggad." Without a
    /// connection, or before the engine's first status, it says why it cannot tell.
    /// </summary>
    private void RefreshEmpty(StackPanel panel, Border badge, TextBlock text, bool show, bool connected, bool older)
    {
        Show(panel, show);
        var known = connected && _engine.Status is not null && !older;
        Show(badge, known);
        SetText(text,
            !connected ? NoContactText
            : _engine.Status is null ? Lang.T("Hämtar kontot …", "Fetching the account …")
            : older ? Lang.T("Uppdatera motorn för att logga in här.", "Update the engine to sign in here.")
            : Lang.T("Inte inloggad.", "Not signed in."));
    }

    private static List<AccountInfo> Shown(AccountInfo? account) =>
        account is { } found && (found.SignedIn || found.SigningIn)
            ? new List<AccountInfo> { found }
            : new List<AccountInfo>();

    /// <summary>Every account the engine reports. An engine without `accounts` (older) reports ACC only.</summary>
    private List<AccountInfo> ReportedAccounts()
    {
        var list = new List<AccountInfo>();
        if (_engine.Status?.Accounts is not null)
        {
            list.AddRange(_engine.Accounts.Where(account => account is not null));
        }
        else if (_engine.AccountFor(ProviderNames.Acc) is { } acc)
        {
            list.Add(acc);
        }
        return list;
    }

    // -------------------------------------------------------------------- cards

    /// <summary>The key of an account's card: its provider and, for StreamBIM, its server key.</summary>
    private static string CardKey(string provider, string? account) => ProviderNames.Id(provider) + "/" + (account ?? string.Empty);

    /// <summary>
    /// Shows one card per account, in the engine's order. A card that is already on screen is
    /// updated in place; a card of an account that is gone leaves the page.
    /// </summary>
    private void ShowCards(StackPanel host, string provider, IReadOnlyList<AccountInfo> accounts)
    {
        var wanted = new List<AccountCard>();
        foreach (var account in accounts)
        {
            var key = CardKey(provider, account.Account);
            if (!_cards.TryGetValue(key, out var card))
            {
                card = BuildCard(host, provider, account.Account);
                _cards[key] = card;
            }

            UpdateCard(card, account);
            wanted.Add(card);
        }

        foreach (var stale in _cards.Where(pair => pair.Value.Host == host && !wanted.Contains(pair.Value)).ToList())
        {
            host.Children.Remove(stale.Value.Card);
            _cards.Remove(stale.Key);
        }

        // An element moves only when it stands out of place, so the focus of an untouched card survives.
        for (var index = 0; index < wanted.Count; index++)
        {
            var element = wanted[index].Card;
            if (index < host.Children.Count && ReferenceEquals(host.Children[index], element))
            {
                continue;
            }

            var current = host.Children.IndexOf(element);
            if (current >= 0)
            {
                host.Children.RemoveAt(current);
            }
            host.Children.Insert(index, element);
        }
    }

    /// <summary>
    /// One account's card: the person (or the account's name), the server with the e-mail, the state, the
    /// subscribed projects with "Visa projekt", and the buttons (Ange kod, Avbryt, Logga ut).
    /// </summary>
    private AccountCard BuildCard(StackPanel host, string provider, string? account)
    {
        var picture = new PersonPicture { Width = 32, Height = 32 };
        var title = new TextBlock { VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap };
        var header = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12 };
        header.Children.Add(picture);
        header.Children.Add(title);

        var server = new TextBlock { Style = (Style)Resources["MutedTextStyle"] };
        var glyph = new FontIcon { FontSize = 12 };
        var ring = new ProgressRing { Width = 12, Height = 12, IsActive = false, Visibility = Visibility.Collapsed };
        var status = new TextBlock { VerticalAlignment = VerticalAlignment.Center };
        var statusRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        statusRow.Children.Add(glyph);
        statusRow.Children.Add(ring);
        statusRow.Children.Add(status);

        var projects = new TextBlock { VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap };
        var showProjects = new HyperlinkButton { Content = Lang.T("Visa projekt", "Show projects"), Padding = new Thickness(0, 0, 4, 0) };
        var projectsRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        projectsRow.Children.Add(projects);
        projectsRow.Children.Add(showProjects);

        var description = new StackPanel { Spacing = 4 };
        description.Children.Add(server);
        description.Children.Add(statusRow);
        description.Children.Add(projectsRow);

        var enterCode = new Button { Content = Lang.T("Ange kod", "Enter code") };
        var cancel = new Button { Content = Lang.T("Avbryt", "Cancel") };
        var signOut = new Button { Content = Lang.T("Logga ut", "Sign out") };
        var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        actions.Children.Add(enterCode);
        actions.Children.Add(cancel);
        actions.Children.Add(signOut);

        var card = new SettingsCard { Header = header, Description = description, Content = actions };
        var result = new AccountCard
        {
            Provider = ProviderNames.Id(provider),
            AccountKey = account,
            Card = card,
            Host = host,
            Picture = picture,
            Title = title,
            Server = server,
            StatusGlyph = glyph,
            StatusRing = ring,
            Status = status,
            Projects = projects,
            ShowProjects = showProjects,
            EnterCode = enterCode,
            Cancel = cancel,
            SignOut = signOut,
        };

        showProjects.Click += (_, _) => ShowAccountProjects(result);
        enterCode.Click += (_, _) => OnEnterCode(result);
        cancel.Click += async (_, _) => await CancelCardSignInAsync(result);
        signOut.Click += async (_, _) => await ConfirmSignOutAsync(result);
        return result;
    }

    private void UpdateCard(AccountCard card, AccountInfo account)
    {
        card.Latest = account;
        var connected = _engine.IsConnected;
        var user = account.User;
        var name = Trimmed(user?.Name);
        var email = Trimmed(user?.Email);
        var platform = AccountName(account);
        var title = name ?? email ?? platform;

        if (card.Picture.DisplayName != (name ?? email ?? string.Empty))
        {
            card.Picture.DisplayName = name ?? email ?? string.Empty;
        }
        SetText(card.Title, title);

        var parts = new List<string>();
        if (email is not null && email != title)
        {
            parts.Add(email);
        }
        parts.Add(ServerText(account));
        SetText(card.Server, string.Join(" · ", parts));

        // The state. A StreamBIM account that asks for a code is waiting for the user, not for the engine.
        var signedIn = account.SignedIn;
        var waitingForCode = WaitsForCode(account);
        string text;
        string glyph;
        Style style;
        if (signedIn)
        {
            text = Lang.T("Inloggad", "Signed in");
            glyph = "";
            style = (Style)Resources["StatusSuccessStyle"];
        }
        else if (waitingForCode)
        {
            text = Lang.T("Väntar på kod", "Waiting for the code");
            glyph = "";
            style = (Style)Resources["StatusCautionStyle"];
        }
        else if (ProviderNames.Id(account.Provider) == ProviderNames.StreamBim)
        {
            text = Lang.T("Loggar in …", "Signing in …");
            glyph = string.Empty;
            style = (Style)Resources["StatusCautionStyle"];
        }
        else
        {
            text = Lang.T("Väntar på webbläsaren …", "Waiting for the browser …");
            glyph = string.Empty;
            style = (Style)Resources["StatusCautionStyle"];
        }

        SetText(card.Status, text);
        if (card.Status.Style != style)
        {
            card.Status.Style = style;
        }
        if (card.StatusGlyph.Glyph != glyph)
        {
            card.StatusGlyph.Glyph = glyph;
        }
        Show(card.StatusGlyph, glyph.Length > 0);
        var spinning = !signedIn && !waitingForCode;
        card.StatusRing.IsActive = spinning;
        Show(card.StatusRing, spinning);

        var count = SubscribedCount(account);
        SetText(card.Projects, count == 1
            ? Lang.T("1 projekt synkas", "1 project synced")
            : Lang.T($"{count} projekt synkas", $"{count} projects synced"));
        Show(card.ShowProjects, signedIn);

        Show(card.SignOut, signedIn);
        card.SignOut.IsEnabled = connected;
        AutomationProperties.SetName(card.SignOut, Lang.T($"Logga ut från {platform}", $"Sign out of {platform}"));
        Show(card.Cancel, !signedIn && account.SigningIn);
        card.Cancel.IsEnabled = connected;
        AutomationProperties.SetName(card.Cancel, Lang.T($"Avbryt inloggningen på {platform}", $"Cancel the sign-in to {platform}"));
        Show(card.EnterCode, waitingForCode);
        card.EnterCode.IsEnabled = connected;
        AutomationProperties.SetName(card.EnterCode, Lang.T($"Ange koden från autentiseringsappen för {platform}", $"Enter the code from the authenticator app for {platform}"));
    }

    /// <summary>The account's name for the card and the dialogs: "ACC", "Interaxo", or the StreamBIM server's name.</summary>
    private string AccountName(AccountInfo account) => ProviderNames.Id(account.Provider) switch
    {
        ProviderNames.StreamBim => ProviderNames.StreamBimName(account, _engine.StreamBimRegions),
        ProviderNames.Interaxo => "Interaxo",
        _ => "ACC",
    };

    /// <summary>The name with the person: "StreamBIM · Anna Andersson". Used where a line must say whose account it is.</summary>
    private string LabelWithPerson(AccountInfo account)
    {
        var who = Trimmed(account.User?.Name) ?? Trimmed(account.User?.Email);
        return who is null ? AccountName(account) : $"{AccountName(account)} · {who}";
    }

    /// <summary>The server of an account in words: "Autodesk", "Interaxo", "Europa · app.streambim.com" or the host of a custom address.</summary>
    private string ServerText(AccountInfo account)
    {
        switch (ProviderNames.Id(account.Provider))
        {
            case ProviderNames.StreamBim:
            {
                var host = ServerHost(account);
                var key = account.Account ?? string.Empty;
                var title = key.Contains('.') ? null : RegionTitle(key);
                return title is null ? host : $"{title} · {host}";
            }

            case ProviderNames.Interaxo:
                return "Interaxo";

            default:
                return "Autodesk";
        }
    }

    /// <summary>The host of a StreamBIM account: the region's `<subdomain>.streambim.com` or a custom address's host.</summary>
    private static string ServerHost(AccountInfo account) =>
        string.IsNullOrWhiteSpace(account.Host) ? account.Account ?? string.Empty : account.Host;

    /// <summary>The host of a StreamBIM account by its key, for the words the dialogs show.</summary>
    private string ServerHostFor(string account) =>
        _engine.StreamBimAccounts.FirstOrDefault(info => info.Account == account) is { } found
            ? ServerHost(found)
            : account;

    /// <summary>A region's title from the engine's list, or null when the list does not name it.</summary>
    private string? RegionTitle(string subdomain)
    {
        var title = _engine.StreamBimRegions?.Regions?
            .FirstOrDefault(region => region is not null && region.Subdomain == subdomain)?.Title;
        return string.IsNullOrWhiteSpace(title) ? null : title.Trim();
    }

    /// <summary>The engine waits for this StreamBIM account's code: its latest progress says mfa_required.</summary>
    private bool WaitsForCode(AccountInfo account) =>
        ProviderNames.Id(account.Provider) == ProviderNames.StreamBim
        && !account.SignedIn
        && account.Account is { } key
        && _engine.SignInProgressFor(ProviderNames.StreamBim, key)?.State == "mfa_required";

    /// <summary>The subscribed projects of an account's hubs. ACC: its hubs. StreamBIM: the hub of its server. Interaxo: its communities.</summary>
    private int SubscribedCount(AccountInfo account) =>
        HubsOf(account).Sum(hub => hub.Projects?.Count(project => project is not null && project.Subscribed) ?? 0);

    private IEnumerable<HubInfo> HubsOf(AccountInfo account)
    {
        var hubs = _engine.Projects?.Hubs ?? new List<HubInfo>();
        var provider = ProviderNames.Id(account.Provider);
        if (provider == ProviderNames.StreamBim)
        {
            var hubId = "streambim:" + account.Account;
            return hubs.Where(hub => hub is not null && hub.Id == hubId);
        }

        return hubs.Where(hub => hub is not null && ProviderNames.Id(hub.Provider) == provider);
    }

    /// <summary>"Visa projekt": the Projects page, showing only this account's hubs.</summary>
    private void ShowAccountProjects(AccountCard card)
    {
        if (card.Latest is not { } account)
        {
            return;
        }

        var hubIds = HubsOf(account).Select(hub => hub.Id).ToList();
        MainWindow.Instance?.NavigateTo(MainWindow.ProjectsTag, new ProjectsFilter(hubIds, LabelWithPerson(account)));
    }

    // ----------------------------------------------------------------- actions

    private void OnAccSignIn(object sender, RoutedEventArgs args) => StartBrowserSignIn(ProviderNames.Acc);

    private void OnInteraxoSignIn(object sender, RoutedEventArgs args) => StartBrowserSignIn(ProviderNames.Interaxo);

    private void OnAddStreamBimServer(object sender, RoutedEventArgs args)
    {
        if (_dialogOpen || _streamBimSignIn is not null)
        {
            return;
        }

        _ = RunStreamBimSignInAsync(null);
    }

    private void OnEnterCode(AccountCard card)
    {
        if (card.AccountKey is not { Length: > 0 } key || _dialogOpen || _streamBimSignIn is not null)
        {
            return;
        }

        _dismissedCodePrompts.Remove(key);
        _ = RunStreamBimSignInAsync(key);
    }

    /// <summary>Ends an account's sign-in while it waits (Avbryt). The engine reports `cancelled`.</summary>
    private async Task CancelCardSignInAsync(AccountCard card)
    {
        var ack = card.Provider == ProviderNames.StreamBim
            ? await _engine.CancelSignInAsync(ProviderNames.StreamBim, card.AccountKey)
            : await _engine.CancelSignInAsync(card.Provider);
        ReportIfFailed(ack, Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        RefreshAll();
    }

    /// <summary>Signs out after the user has confirmed it. The projects pause, the folders stay.</summary>
    private async Task ConfirmSignOutAsync(AccountCard card)
    {
        if (_dialogOpen || card.Latest is not { } account || XamlRoot is not { } root)
        {
            return;
        }

        var name = AccountName(account);
        var dialog = NewDialog(root, Lang.T($"Logga ut från {name}?", $"Sign out of {name}?"),
            Line(Lang.T(
                "Projekten pausas tills du loggar in igen. Mapparna ligger kvar.",
                "The projects pause until you sign in again. The folders stay.")));
        dialog.PrimaryButtonText = Lang.T("Logga ut", "Sign out");
        dialog.CloseButtonText = Lang.T("Avbryt", "Cancel");
        // Enter must not sign anyone out by accident.
        dialog.DefaultButton = ContentDialogButton.Close;

        if (await ShowDialogAsync(dialog) != ContentDialogResult.Primary)
        {
            return;
        }

        var ack = card.Provider == ProviderNames.StreamBim
            ? await _engine.SignOutAsync(ProviderNames.StreamBim, card.AccountKey)
            : await _engine.SignOutAsync(card.Provider);
        ReportIfFailed(ack, Lang.T("Utloggningen gick inte igenom", "Sign-out did not go through"));
        RefreshAll();
    }

    // ------------------------------------------------------------------ helpers

    /// <summary>A ContentDialog of this page: in the window's theme, with the window as its root.</summary>
    private static ContentDialog NewDialog(XamlRoot root, string title, object content) => new()
    {
        XamlRoot = root,
        // The dialog hangs in the popup root and does not inherit the theme choice.
        RequestedTheme = MainWindow.CurrentTheme,
        Title = title,
        Content = content,
    };

    /// <summary>
    /// Shows a dialog and returns its answer. Only one dialog of the page is open at a time. A dialog
    /// that cannot open (another is showing) gives None.
    /// </summary>
    private async Task<ContentDialogResult> ShowDialogAsync(ContentDialog dialog)
    {
        _dialogOpen = true;
        try
        {
            return await dialog.ShowAsync();
        }
        catch (Exception exception)
        {
            DiagnosticLog.Write("kontodialog", exception);
            return ContentDialogResult.None;
        }
        finally
        {
            _dialogOpen = false;
        }
    }

    private static TextBlock Line(string text) => new() { Text = text, TextWrapping = TextWrapping.Wrap };

    /// <summary>Starts a browser sign-in (ACC, Interaxo): the dialog that waits for the browser, and the engine's sign_in.</summary>
    private void StartBrowserSignIn(string provider)
    {
        if (_dialogOpen || _browserSignIn is not null)
        {
            return;
        }

        DismissFailure(provider);
        _ = RunBrowserSignInAsync(provider);
    }

    private async Task RunBrowserSignInAsync(string provider)
    {
        if (XamlRoot is not { } root)
        {
            return;
        }

        var session = new BrowserSignIn(this, root, provider);
        _browserSignIn = session;
        var closed = session.ShowAsync();
        var ack = await _engine.SignInAsync(provider);
        if (!ack.Ok && !session.Fail(ack.Error ?? Lang.T("Inloggningen gick inte att starta.", "Sign-in could not be started.")))
        {
            ReportIfFailed(ack, Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
        }

        await closed;
        if (ReferenceEquals(_browserSignIn, session))
        {
            _browserSignIn = null;
        }
        RefreshAll();
    }

    /// <summary>Opens the StreamBIM dialog: the form, or at the code step for an account that waits for its code.</summary>
    private async Task RunStreamBimSignInAsync(string? codeFor)
    {
        if (XamlRoot is not { } root)
        {
            return;
        }

        var session = new StreamBimSignIn(this, root);
        _streamBimSignIn = session;
        await session.ShowAsync(codeFor);
        if (ReferenceEquals(_streamBimSignIn, session))
        {
            _streamBimSignIn = null;
        }
        RefreshAll();
        ShowCodePromptIfWanted();
    }

    /// <summary>
    /// Opens the code step for an account that waits for its code, when no dialog is open and the user
    /// has not closed that prompt. Several accounts waiting: their prompts come one after the other.
    /// </summary>
    private void ShowCodePromptIfWanted()
    {
        if (!_attached || _dialogOpen || _streamBimSignIn is not null)
        {
            return;
        }

        if (StreamBimAccountWaitingForCode() is { } key)
        {
            _ = RunStreamBimSignInAsync(key);
        }
    }

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
            var dismissed = _dismissedCodePrompts.TryGetValue(account.Account, out var closed) && ReferenceEquals(closed, progress);
            if (progress is { State: "mfa_required" } && !dismissed)
            {
                return account.Account;
            }
        }
        return null;
    }

    /// <summary>
    /// Shows a provider's failed sign-in in its section, once. A failure the user has closed or retried
    /// stays hidden until the engine reports a new one. A cancelled sign-in shows only when the engine says why.
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

    /// <summary>The user closed a failure bar: it stays closed until the engine reports a new failure.</summary>
    private void OnFailureClosed(InfoBar sender, InfoBarClosedEventArgs args)
    {
        if (sender.Tag is not string provider)
        {
            return;
        }

        if (provider == ProviderNames.StreamBim)
        {
            if (_streamBimFailureKey is not null)
            {
                DismissFailure(provider, _streamBimFailureKey);
            }
        }
        else
        {
            DismissFailure(provider);
        }
    }

    /// <summary>Shows a refused command in the window's message bar, with the engine's reason.</summary>
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

    private static string? Trimmed(string? text) => string.IsNullOrWhiteSpace(text) ? null : text.Trim();

    private static void SetText(TextBlock block, string text)
    {
        if (block.Text != text)
        {
            block.Text = text;
        }
    }

    private static void Show(UIElement element, bool visible)
    {
        var wanted = visible ? Visibility.Visible : Visibility.Collapsed;
        if (element.Visibility != wanted)
        {
            element.Visibility = wanted;
        }
    }

    /// <summary>One account's card on screen and the account it shows (set on each refresh).</summary>
    private sealed class AccountCard
    {
        public required string Provider { get; init; }

        /// <summary>The StreamBIM server key; null for ACC and Interaxo.</summary>
        public string? AccountKey { get; init; }

        public required SettingsCard Card { get; init; }

        /// <summary>The section the card is shown in.</summary>
        public required StackPanel Host { get; init; }

        public required PersonPicture Picture { get; init; }

        public required TextBlock Title { get; init; }

        public required TextBlock Server { get; init; }

        public required FontIcon StatusGlyph { get; init; }

        public required ProgressRing StatusRing { get; init; }

        public required TextBlock Status { get; init; }

        public required TextBlock Projects { get; init; }

        public required HyperlinkButton ShowProjects { get; init; }

        public required Button EnterCode { get; init; }

        public required Button Cancel { get; init; }

        public required Button SignOut { get; init; }

        /// <summary>The account as the engine reported it last. Null until the first refresh.</summary>
        public AccountInfo? Latest { get; set; }
    }
}
