using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace Synka.Views;

/// <summary>
/// The sign-in dialogs of Konton. One sign-in has one dialog: the browser wait of ACC and
/// Interaxo, and the StreamBIM server form with its code step. The engine's progress for the
/// sign-in moves the dialog; the dialog never sends a password anywhere but `sign_in`.
/// </summary>
public sealed partial class AccountsPage
{
    /// <summary>The text of a sign-in failure, in the window's words.</summary>
    private static string SignInFailureText(string? error)
    {
        var text = ErrorDisplay.FriendlyDetail(error);
        return text.Length > 0 ? text : Lang.T("Inloggningen misslyckades.", "Sign-in failed.");
    }

    /// <summary>
    /// "Fortsätt i webbläsaren": one browser sign-in (ACC or Interaxo) while the browser has it. It
    /// closes itself when the engine says the sign-in is over. A failure stays on screen until Stäng.
    /// </summary>
    private sealed class BrowserSignIn
    {
        private readonly AccountsPage _page;
        private readonly ContentDialog _dialog;
        private readonly ProgressRing _ring;
        private readonly InfoBar _error;

        /// <summary>The engine ended the sign-in, or the dialog was closed. Nothing is cancelled then.</summary>
        private bool _ended;

        /// <summary>The dialog has closed. A late answer of the engine then has no dialog to show in.</summary>
        private bool _closed;

        /// <summary>A failure is on screen; the dialog waits for Stäng.</summary>
        private bool _failed;

        public BrowserSignIn(AccountsPage page, XamlRoot root, string provider)
        {
            _page = page;
            Provider = provider;

            _ring = new ProgressRing { IsActive = true, Width = 24, Height = 24 };
            AutomationProperties.SetName(_ring, Lang.T("Väntar på webbläsaren", "Waiting for the browser"));
            _error = new InfoBar { Severity = InfoBarSeverity.Error, IsOpen = false, IsClosable = false };

            var body = new StackPanel { Spacing = 14 };
            body.Children.Add(Line(Lang.T(
                "Synka har öppnat inloggningen i din webbläsare. Kom tillbaka hit när du är klar.",
                "Synka has opened the sign-in in your browser. Come back here when you are done.")));
            body.Children.Add(_ring);
            body.Children.Add(_error);

            _dialog = NewDialog(root, Lang.T("Fortsätt i webbläsaren", "Continue in the browser"), body);
            _dialog.CloseButtonText = Lang.T("Avbryt", "Cancel");
            _dialog.DefaultButton = ContentDialogButton.Close;
        }

        /// <summary>The provider of the sign-in: ACC or Interaxo.</summary>
        public string Provider { get; }

        /// <summary>Shows the dialog until it closes. A close while the browser still waits cancels the sign-in.</summary>
        public async Task ShowAsync()
        {
            await _page.ShowDialogAsync(_dialog);
            _closed = true;
            if (_ended || _failed)
            {
                return;
            }

            _ended = true;
            ReportIfFailed(await _page._engine.CancelSignInAsync(Provider),
                Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        }

        /// <summary>The engine refused the sign-in. Returns false when the dialog is gone, so the caller reports it instead.</summary>
        public bool Fail(string message)
        {
            if (_ended || _closed)
            {
                return false;
            }

            ShowFailure(message);
            return true;
        }

        /// <summary>The engine's progress for this provider: signed_in and cancelled close the dialog; failed shows its reason.</summary>
        public void OnProgress(SignInProgressInfo progress)
        {
            switch (progress.State)
            {
                case "signed_in":
                case "cancelled":
                    End();
                    break;

                case "failed":
                    // The section does not repeat what the dialog shows.
                    _page.DismissFailure(Provider);
                    ShowFailure(SignInFailureText(progress.Error));
                    break;
            }
        }

        private void End()
        {
            if (_ended)
            {
                return;
            }

            _ended = true;
            _dialog.Hide();
        }

        private void ShowFailure(string message)
        {
            _failed = true;
            _ring.IsActive = false;
            _ring.Visibility = Visibility.Collapsed;
            _error.Message = message;
            _error.IsOpen = true;
            _dialog.CloseButtonText = Lang.T("Stäng", "Close");
        }
    }

    /// <summary>
    /// "Lägg till StreamBIM-server": the server, the e-mail and the password, then the six-digit code
    /// an account asks for. The dialog stays open while the engine works; the engine's progress moves
    /// it between the form, the code step and its close.
    /// </summary>
    private sealed class StreamBimSignIn
    {
        /// <summary>The server list's last item: a custom https address. Not a subdomain.</summary>
        private const string OtherServerTag = "#other";

        private readonly AccountsPage _page;
        private readonly ContentDialog _dialog;
        private readonly StackPanel _form;
        private readonly StackPanel _codeStep;
        private readonly ComboBox _server;
        private readonly TextBox _url;
        private readonly TextBlock _urlWarning;
        private readonly TextBlock _urlError;
        private readonly TextBox _email;
        private readonly PasswordBox _password;
        private readonly StackPanel _sending;
        private readonly InfoBar _formError;
        private readonly TextBox _code;
        private readonly InfoBar _codeError;

        /// <summary>The StreamBIM account the engine is signing in: the attempt the dialog follows. Null when none.</summary>
        private string? _attempt;

        /// <summary>True on the code step: the engine asked for a code.</summary>
        private bool _onCode;

        /// <summary>The code went to the engine: Verifiera waits for the engine's next progress.</summary>
        private bool _codeSent;

        /// <summary>The engine ended the sign-in, or the dialog was told to close. Nothing is cancelled then.</summary>
        private bool _ended;

        /// <summary>The dialog has closed. A late answer of the engine then has no dialog to show in.</summary>
        private bool _closed;

        /// <summary>True while the server list is rebuilt: its selection is then not the user's choice.</summary>
        private bool _populating;

        /// <summary>The server the user picked last. Kept when the list changes.</summary>
        private string? _choice;

        /// <summary>
        /// The engine has not answered the attempt yet (no progress for it has come). A warning
        /// notify then refuses it: the engine refuses an account that is signed in already that way.
        /// </summary>
        private bool _awaitingAnswer;

        public StreamBimSignIn(AccountsPage page, XamlRoot root)
        {
            _page = page;

            _server = new ComboBox
            {
                Header = Lang.T("Server", "Server"),
                PlaceholderText = Lang.T("Hämtar regioner …", "Fetching regions …"),
                HorizontalAlignment = HorizontalAlignment.Stretch,
            };
            AutomationProperties.SetName(_server, Lang.T("StreamBIM-server", "StreamBIM server"));
            _server.SelectionChanged += OnServerChanged;

            _url = new TextBox
            {
                Header = Lang.T("Egen adress", "Your own address"),
                PlaceholderText = "https://streambim.exempel.se",
                Visibility = Visibility.Collapsed,
                AcceptsReturn = false,
                IsSpellCheckEnabled = false,
                IsTextPredictionEnabled = false,
                MaxLength = 512,
            };
            AutomationProperties.SetName(_url, Lang.T("StreamBIM-adress", "StreamBIM address"));
            _url.TextChanged += (_, _) => Refresh();

            _urlWarning = new TextBlock
            {
                Text = Lang.T(
                    "Lösenordet skickas till den här adressen. Använd bara adresser du litar på.",
                    "The password is sent to this address. Use only addresses you trust."),
                Style = (Style)page.Resources["CaptionCautionStyle"],
                Visibility = Visibility.Collapsed,
            };
            _urlError = new TextBlock
            {
                Style = (Style)page.Resources["CaptionCriticalStyle"],
                Visibility = Visibility.Collapsed,
            };

            _email = new TextBox
            {
                Header = Lang.T("E-post", "E-mail"),
                MaxLength = 256,
                AcceptsReturn = false,
                IsSpellCheckEnabled = false,
                IsTextPredictionEnabled = false,
            };
            AutomationProperties.SetName(_email, Lang.T("StreamBIM e-post", "StreamBIM e-mail"));
            _email.TextChanged += (_, _) => Refresh();

            _password = new PasswordBox { Header = Lang.T("Lösenord", "Password") };
            AutomationProperties.SetName(_password, Lang.T("StreamBIM lösenord", "StreamBIM password"));
            _password.PasswordChanged += (_, _) => Refresh();

            _sending = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Visibility = Visibility.Collapsed };
            _sending.Children.Add(new ProgressRing { IsActive = true, Width = 16, Height = 16 });
            _sending.Children.Add(new TextBlock { Text = Lang.T("Loggar in …", "Signing in …") });

            _formError = new InfoBar { Severity = InfoBarSeverity.Error, IsOpen = false, IsClosable = true };
            _form = new StackPanel { Spacing = 10, MinWidth = 300 };
            _form.Children.Add(_server);
            _form.Children.Add(_url);
            _form.Children.Add(_urlWarning);
            _form.Children.Add(_urlError);
            _form.Children.Add(_email);
            _form.Children.Add(_password);
            _form.Children.Add(_formError);
            _form.Children.Add(_sending);

            _code = new TextBox
            {
                Header = Lang.T("Verifieringskod", "Verification code"),
                PlaceholderText = "000000",
                MaxLength = 6,
                IsSpellCheckEnabled = false,
                IsTextPredictionEnabled = false,
            };
            AutomationProperties.SetName(_code, Lang.T("Verifieringskod, sex siffror", "Verification code, six digits"));
            _code.TextChanged += OnCodeChanged;
            _codeError = new InfoBar { Severity = InfoBarSeverity.Error, IsOpen = false, IsClosable = true };
            _codeStep = new StackPanel { Spacing = 10, MinWidth = 300 };
            _codeStep.Children.Add(Line(Lang.T("Sex siffror från din autentiseringsapp.", "Six digits from your authenticator app.")));
            _codeStep.Children.Add(_code);
            _codeStep.Children.Add(_codeError);

            _dialog = NewDialog(root, Lang.T("Lägg till StreamBIM-server", "Add a StreamBIM server"), _form);
            _dialog.PrimaryButtonText = Lang.T("Logga in", "Sign in");
            _dialog.CloseButtonText = Lang.T("Avbryt", "Cancel");
            _dialog.DefaultButton = ContentDialogButton.Primary;
            _dialog.IsPrimaryButtonEnabled = false;
            _dialog.PrimaryButtonClick += OnPrimaryClick;

            FillServers();
        }

        /// <summary>Shows the form, or the code step for the account that asks for its code.</summary>
        public async Task ShowAsync(string? codeFor)
        {
            if (codeFor is null)
            {
                ShowForm();
            }
            else
            {
                ShowCode(codeFor);
            }

            _page._engine.Notified += OnNotified;
            await _page.ShowDialogAsync(_dialog);
            _page._engine.Notified -= OnNotified;
            _closed = true;
            if (_ended || _attempt is not { } key)
            {
                return;
            }

            // The user closed the dialog while the engine works on this account: its sign-in ends.
            _attempt = null;
            if (_onCode && _page._engine.SignInProgressFor(ProviderNames.StreamBim, key) is { } prompt)
            {
                _page._dismissedCodePrompts[key] = prompt;
            }

            ReportIfFailed(await _page._engine.CancelSignInAsync(ProviderNames.StreamBim, key),
                Lang.T("Inloggningen gick inte att avbryta", "The sign-in could not be cancelled"));
        }

        /// <summary>The engine's progress for a StreamBIM account. Only the attempt this dialog follows moves it.</summary>
        public void OnProgress(SignInProgressInfo progress)
        {
            if (_attempt is null || progress.Account != _attempt)
            {
                return;
            }

            var key = _attempt;
            _awaitingAnswer = false;
            switch (progress.State)
            {
                case "mfa_required":
                    if (_onCode)
                    {
                        // Another prompt for the same attempt: the code is asked again.
                        _codeSent = false;
                        _code.Text = string.Empty;
                        ShowCodeError(null);
                        Refresh();
                    }
                    else
                    {
                        ShowCode(key);
                    }
                    break;

                case "signed_in":
                    _attempt = null;
                    End();
                    break;

                case "failed":
                    // The user starts over: the e-mail stays, the password is typed again.
                    _attempt = null;
                    _codeSent = false;
                    _page.DismissFailure(ProviderNames.StreamBim, key);
                    ShowForm();
                    ShowFormError(SignInFailureText(progress.Error));
                    Refresh();
                    break;

                case "cancelled":
                    _attempt = null;
                    _codeSent = false;
                    ShowForm();
                    if (!string.IsNullOrEmpty(progress.Error))
                    {
                        _page.DismissFailure(ProviderNames.StreamBim, key);
                        ShowFormError(ErrorDisplay.FriendlyDetail(progress.Error));
                    }
                    Refresh();
                    break;
            }
        }

        /// <summary>
        /// A warning or error notify while the form waits for the engine's answer: the engine refused the
        /// sign-in (an account already signed in that way, say). The form is editable again, with the reason.
        /// </summary>
        private void OnNotified(object? sender, NotifyInfo notify)
        {
            if (_onCode || !_awaitingAnswer || _attempt is null || notify.Severity is not ("warning" or "error"))
            {
                return;
            }

            _attempt = null;
            _awaitingAnswer = false;
            var text = ErrorDisplay.FriendlyDetail(notify.Body);
            ShowFormError(text.Length > 0 ? text : Lang.T("Inloggningen gick inte att starta.", "Sign-in could not be started."));
            Refresh();
        }

        /// <summary>The engine sent its region list: the server list follows it, unless the code step is showing.</summary>
        public void RegionsChanged()
        {
            if (!_onCode)
            {
                FillServers();
            }
        }

        /// <summary>The connection to the engine is gone, and the engine's code wait with it: the dialog closes.</summary>
        public void CloseForDisconnect()
        {
            _attempt = null;
            End();
        }

        /// <summary>Writes the engine's state and the checks of the fields into the dialog. Runs with every engine message.</summary>
        public void Refresh()
        {
            if (_ended)
            {
                return;
            }

            var connected = _page._engine.IsConnected;
            if (_onCode)
            {
                _code.IsEnabled = connected && !_codeSent;
                _dialog.IsPrimaryButtonEnabled = connected && !_codeSent && _code.Text.Length == 6;
                return;
            }

            // An engine without `accounts` (older) takes no StreamBIM sign-in: the form waits for an update.
            var older = _page._engine.Status is { Accounts: null };
            var editable = connected && _page._engine.Status is not null && !older && _attempt is null;
            var signedIn = _page._engine.StreamBimAccounts
                .Where(account => account.SignedIn && !string.IsNullOrEmpty(account.Account))
                .Select(account => account.Account!)
                .ToHashSet(StringComparer.Ordinal);

            // A server that is signed in cannot be picked again; it says so.
            foreach (var item in _server.Items.OfType<ComboBoxItem>())
            {
                if (item.Tag is not string tag || tag == OtherServerTag)
                {
                    continue;
                }

                var taken = signedIn.Contains(tag);
                var content = taken
                    ? RegionName(tag) + " " + Lang.T("(inloggad)", "(signed in)")
                    : RegionName(tag);
                if (!Equals(item.Content, content))
                {
                    item.Content = content;
                }
                item.IsEnabled = !taken;
            }
            EnsureServerPicked();

            var other = IsOtherPicked();
            Show(_url, other);
            Show(_urlWarning, other);

            var urlReady = false;
            string? urlError = null;
            if (other)
            {
                var typed = _url.Text.Trim();
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
            SetText(_urlError, urlError ?? string.Empty);
            Show(_urlError, urlError is not null);

            _server.IsEnabled = editable;
            _url.IsEnabled = editable;
            _email.IsEnabled = editable;
            _password.IsEnabled = editable;
            Show(_sending, _attempt is not null);

            _dialog.IsPrimaryButtonEnabled = editable
                && _email.Text.Trim().Length > 0
                && _password.Password.Length > 0
                && (other ? urlReady : SelectedRegion() is not null);
        }

        private void ShowForm()
        {
            _onCode = false;
            _dialog.Content = _form;
            _dialog.Title = Lang.T("Lägg till StreamBIM-server", "Add a StreamBIM server");
            _dialog.PrimaryButtonText = Lang.T("Logga in", "Sign in");
            Refresh();
        }

        private void ShowCode(string key)
        {
            _attempt = key;
            _onCode = true;
            _codeSent = false;
            _code.Text = string.Empty;
            ShowCodeError(null);
            var host = _page.ServerHostFor(key);
            _dialog.Content = _codeStep;
            _dialog.Title = Lang.T(
                $"Ange koden från autentiseringsappen för {host}",
                $"Enter the code from the authenticator app for {host}");
            _dialog.PrimaryButtonText = Lang.T("Verifiera", "Verify");
            Refresh();
        }

        private void End()
        {
            if (_ended)
            {
                return;
            }

            _ended = true;
            _dialog.Hide();
        }

        /// <summary>
        /// Primary: the form sends the sign-in, the code step sends the code. The dialog stays open
        /// either way; it closes when the engine says the sign-in is over.
        /// </summary>
        private async void OnPrimaryClick(ContentDialog sender, ContentDialogButtonClickEventArgs args)
        {
            args.Cancel = true;
            if (_onCode)
            {
                await SubmitCodeAsync();
            }
            else
            {
                await SubmitFormAsync();
            }
        }

        /// <summary>
        /// Sends the sign-in: exactly one of the region and the custom address, with the e-mail and the
        /// password. The password box is emptied as it is sent, and nothing keeps it afterwards.
        /// </summary>
        private async Task SubmitFormAsync()
        {
            if (_attempt is not null || !_page._engine.IsConnected)
            {
                return;
            }

            string? region = null;
            string? url = null;
            string key;
            string host;
            if (IsOtherPicked())
            {
                if (!StreamBimAddress.TryParse(_url.Text, out var address, out _))
                {
                    Refresh();
                    return;
                }
                url = address.Url;
                key = address.Key;
                host = address.Host;
            }
            else if (SelectedRegion() is { } chosen)
            {
                region = chosen;
                key = chosen;
                host = chosen + ".streambim.com";
            }
            else
            {
                return;
            }

            var username = _email.Text.Trim();
            var password = _password.Password;
            if (username.Length == 0 || password.Length == 0)
            {
                return;
            }

            _password.Password = string.Empty;
            _attempt = key;
            _awaitingAnswer = true;
            _page._streamBimFailureKey = null;
            _page.DismissFailure(ProviderNames.StreamBim, key);
            ShowFormError(null);
            Refresh();

            var ack = await _page._engine.SignInStreamBimAsync(region, url, username, password);
            if (ack.Ok)
            {
                return;
            }

            // Refused (an invalid address, say): the form is back with the reason.
            if (_attempt == key)
            {
                _attempt = null;
                _awaitingAnswer = false;
            }
            if (_closed)
            {
                ReportIfFailed(ack, Lang.T("Inloggningen gick inte att starta", "Sign-in could not be started"));
                return;
            }

            var text = ErrorDisplay.FriendlyDetail(ack.Error);
            ShowFormError(text.Length > 0 ? text : Lang.T("Okänt fel.", "Unknown error."));
            Refresh();
        }

        /// <summary>Sends the typed code once. The engine's next progress answers it.</summary>
        private async Task SubmitCodeAsync()
        {
            if (_attempt is null || _codeSent || _code.Text.Length != 6)
            {
                return;
            }

            var key = _attempt;
            _codeSent = true;
            ShowCodeError(null);
            Refresh();

            var ack = await _page._engine.SignInMfaAsync(ProviderNames.StreamBim, key, _code.Text);
            if (ack.Ok)
            {
                return;
            }

            _codeSent = false;
            if (_closed)
            {
                ReportIfFailed(ack, Lang.T("Koden gick inte att skicka", "The code could not be sent"));
                return;
            }

            var text = ErrorDisplay.FriendlyDetail(ack.Error);
            ShowCodeError(text.Length > 0 ? text : Lang.T("Koden gick inte att skicka.", "The code could not be sent."));
            Refresh();
        }

        /// <summary>Six digits only: the engine refuses anything else.</summary>
        private void OnCodeChanged(object sender, TextChangedEventArgs args)
        {
            var digits = new string(_code.Text.Where(character => character is >= '0' and <= '9').Take(6).ToArray());
            if (digits != _code.Text)
            {
                _code.Text = digits;
                return;
            }
            Refresh();
        }

        private void OnServerChanged(object sender, SelectionChangedEventArgs args)
        {
            if (!_populating && _server.SelectedItem is ComboBoxItem { Tag: string tag } && tag.Length > 0)
            {
                _choice = tag;
            }
            Refresh();
        }

        /// <summary>Fills the server list from the engine's regions, then "Annan adress…". The pick stays when it can still be picked.</summary>
        private void FillServers()
        {
            var regions = (_page._engine.StreamBimRegions?.Regions ?? Array.Empty<StreamBimRegion>())
                .Where(region => region is not null && !string.IsNullOrWhiteSpace(region.Subdomain))
                .ToList();

            _populating = true;
            try
            {
                _server.Items.Clear();
                foreach (var region in regions)
                {
                    _server.Items.Add(new ComboBoxItem { Tag = region.Subdomain });
                }
                _server.Items.Add(new ComboBoxItem
                {
                    Content = Lang.T("Annan adress…", "Other address…"),
                    Tag = OtherServerTag,
                });
            }
            finally
            {
                _populating = false;
            }

            EnsureServerPicked();
            Refresh();
        }

        /// <summary>
        /// Keeps a server that is picked. When the pick is gone or cannot be picked (its server is signed in
        /// now), the picks are tried in order: the server chosen last, the default region, the first region
        /// not signed in, and the own address.
        /// </summary>
        private void EnsureServerPicked()
        {
            if (_server.SelectedItem is ComboBoxItem { IsEnabled: true })
            {
                return;
            }

            var enabled = _server.Items.OfType<ComboBoxItem>().Where(item => item.IsEnabled).ToList();
            var defaultRegion = _page._engine.StreamBimRegions?.Regions?
                .FirstOrDefault(region => region is { IsDefault: true })?.Subdomain;
            var pick = enabled.FirstOrDefault(item => item.Tag is string tag && tag == _choice)
                ?? enabled.FirstOrDefault(item => item.Tag is string tag && tag == defaultRegion)
                ?? enabled.FirstOrDefault(item => item.Tag is string tag && tag != OtherServerTag)
                ?? enabled.FirstOrDefault();

            _populating = true;
            try
            {
                _server.SelectedItem = pick;
            }
            finally
            {
                _populating = false;
            }
        }

        /// <summary>The server the user picked, or null when the own address is picked or nothing is.</summary>
        private string? SelectedRegion() =>
            _server.SelectedItem is ComboBoxItem { Tag: string tag } && tag.Length > 0 && tag != OtherServerTag
                ? tag
                : null;

        private bool IsOtherPicked() => _server.SelectedItem is ComboBoxItem { Tag: OtherServerTag };

        /// <summary>A region's title, or its subdomain when the list does not name it.</summary>
        private string RegionName(string subdomain) => _page.RegionTitle(subdomain) ?? subdomain;

        private void ShowFormError(string? message)
        {
            _formError.Message = message ?? string.Empty;
            _formError.IsOpen = !string.IsNullOrEmpty(message);
        }

        private void ShowCodeError(string? message)
        {
            _codeError.Message = message ?? string.Empty;
            _codeError.IsOpen = !string.IsNullOrEmpty(message);
        }
    }
}
