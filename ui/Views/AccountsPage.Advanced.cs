using System;
using System.Threading.Tasks;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Synka.Views;

/// <summary>
/// The advanced client settings, under "Avancerat" in their section: the optional APS app of ACC
/// (test and save as before, see ApsClientIdEditState) and the Interaxo client (Client ID, redirect
/// URI, optional secret). Both were in Settings; their rules are unchanged.
/// </summary>
public sealed partial class AccountsPage
{
    /// <summary>Five minutes for the browser, bounded token and profile calls, then a reconnect margin.</summary>
    private static readonly TimeSpan ApsClientIdAnswerTimeout = TimeSpan.FromMinutes(8);

    private ApsClientIdEditState ApsEdit => _engine.ApsClientIdEdit;

    /// <summary>True while the page writes the engine's APS client id into its box, so TextChanged is not taken for the user.</summary>
    private bool _apsMirroring;

    /// <summary>Copies the engine's advanced settings into their controls.</summary>
    private void RefreshSettings()
    {
        var settings = _engine.Settings;
        RefreshApsSettings(settings);
        if (settings is null)
        {
            return;
        }

        _mirroring = true;
        try
        {
            RefreshInteraxoSettings(settings);
        }
        finally
        {
            _mirroring = false;
        }
    }

    /// <summary>
    /// Copies the engine's Interaxo client settings into their boxes, unless the user has typed in them
    /// since the last save. The redirect URI shows only when it is not the default: the default is the
    /// box's placeholder, and a blank box saves the default.
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

    // ------------------------------------------------------------- APS app (ACC)

    private void RefreshApsSettings(SettingsInfo? settings)
    {
        var supported = settings?.ApsClientIdOverrideSupported == true;
        AccAdvanced.Visibility = supported ? Visibility.Visible : Visibility.Collapsed;
        if (supported)
        {
            ApsEdit.ObserveSettings(settings!.ApsClientId);
            if (ApsClientIdBox.Text != ApsEdit.Draft)
            {
                _apsMirroring = true;
                try
                {
                    ApsClientIdBox.Text = ApsEdit.Draft;
                }
                finally
                {
                    _apsMirroring = false;
                }
            }
        }
        else
        {
            ApsEdit.FailIfPending(ApsEdit.RequestVersion, null);
        }
        RefreshApsControls();
    }

    /// <summary>The APS editor's controls and status line. ACC's Logga in waits for a test or save in progress (RefreshAll).</summary>
    private void RefreshApsControls()
    {
        var connected = _engine.IsConnected;
        var supported = _engine.Settings?.ApsClientIdOverrideSupported == true;
        var signedOut = _engine.Status is { SignedIn: false };
        var available = connected && supported && signedOut && !_engine.IsSignedIn
            && !_engine.IsFreeingUp && !_engine.IsMovingSyncRoot && !ApsEdit.IsWaiting;
        ApsClientIdBox.IsEnabled = available;
        ApsClientIdSaveButton.IsEnabled = available && ApsEdit.CanSave
            && _engine.Settings?.ApsClientIdValidationSupported == true;
        ApsClientIdDefaultButton.IsEnabled = available && ApsEdit.CanUseDefault;
        ApsClientIdCancelButton.Visibility = ApsEdit.IsTesting ? Visibility.Visible : Visibility.Collapsed;
        ApsClientIdCancelButton.IsEnabled = connected && ApsEdit.IsTesting;
        ApsClientIdProgress.IsActive = ApsEdit.IsWaiting;

        ApsClientIdStatus.Text = !connected
            ? Lang.T("Ingen kontakt med motorn. Ditt utkast behålls.", "No contact with the engine. Your draft is kept.")
            : !signedOut || _engine.IsSignedIn
                ? Lang.T("Logga ut innan du byter APS-app.", "Sign out before changing the APS app.")
                : ApsEdit.IsWaiting
                    ? ApsEdit.IsTesting
                        ? Lang.T("Slutför testinloggningen i webbläsaren. Inställningen sparas bara om PKCE-testet lyckas …", "Complete the test sign-in in your browser. The setting is saved only if the PKCE test succeeds …")
                        : Lang.T("Väntar på att motorn bekräftar ändringen …", "Waiting for the engine to confirm the change …")
                    : ApsEdit.Problem
                        ?? (ApsEdit.Saved
                            ? Lang.T("Inställningen är sparad. Logga in i Synka för att börja synka.", "The setting is saved. Sign in to Synka to start syncing.")
                            : _engine.Settings?.ApsClientIdValidationSupported != true
                            ? Lang.T("Uppdatera motorn för att testa och spara en egen APS-app.", "Update the engine to test and save a custom APS app.")
                            : ApsEdit.HasChanges
                            ? Lang.T("Ändringen är inte sparad.", "The change has not been saved.")
                            : ApsEdit.Confirmed.Length == 0
                                ? Lang.T("Synkas standardapp används.", "The Synka default app is in use.")
                                : Lang.T("En egen APS-app används.", "A custom APS app is in use."));
    }

    private void OnApsClientIdChanged(object sender, TextChangedEventArgs args)
    {
        if (!_attached || _apsMirroring)
        {
            return;
        }

        ApsEdit.Edit(ApsClientIdBox.Text);
        RefreshApsControls();
    }

    private async void OnSaveApsClientId(object sender, RoutedEventArgs args) =>
        await SaveApsClientIdAsync(ApsClientIdBox.Text);

    private async void OnUseDefaultApsClientId(object sender, RoutedEventArgs args) =>
        await SaveApsClientIdAsync(string.Empty);

    private async Task SaveApsClientIdAsync(string value)
    {
        if (ApsEdit.IsWaiting || !_engine.IsConnected || _engine.IsSignedIn
            || _engine.Status is not { SignedIn: false }
            || _engine.Settings?.ApsClientIdOverrideSupported != true
            || _engine.IsFreeingUp || _engine.IsMovingSyncRoot)
        {
            return;
        }

        var normalized = ApsClientIdEditState.Normalize(value);
        var validate = normalized.Length > 0;
        if (validate && _engine.Settings?.ApsClientIdValidationSupported != true)
        {
            return;
        }

        var request = ApsEdit.Begin(normalized, validate);
        var validationId = ApsEdit.RequestId;
        RefreshApsControls();
        var ack = validate
            ? await _engine.TestApsClientIdAsync(normalized, validationId!)
            : await _engine.SetSettingAsync("aps_client_id", normalized);

        // A settings event may confirm the write before the enqueue ACK arrives.
        if (!ApsEdit.IsPending(request))
        {
            return;
        }
        if (ApsEdit.ObserveAck(request, ack))
        {
            ReportIfFailed(ack, Lang.T("APS-inställningen gick inte igenom", "The APS setting did not go through"));
            return;
        }

        // A successful ACK only means enqueued; validation has its own correlated result.
        // A changed settings snapshot also recovers a completion lost during restart.
        await Task.Delay(validate ? ApsClientIdAnswerTimeout : TimeSpan.FromSeconds(30));
        if (ApsEdit.IsPending(request) && validationId is not null)
        {
            await _engine.CancelApsClientIdTestAsync(validationId);
        }
        if (ApsEdit.FailIfPending(request, Lang.T(
                "Motorn har inte bekräftat ändringen. Ditt utkast behålls; kontrollera anslutningen och försök igen.",
                "The engine has not confirmed the change. Your draft is kept; check the connection and try again.")))
        {
            RefreshApsControls();
        }
    }

    private void OnApsClientIdNotified(object? sender, NotifyInfo notify)
    {
        if (!ApsEdit.IsWaiting
            || ApsEdit.IsTesting
            || notify.Severity is not ("warning" or "error")
            || notify.Body?.Contains("APS", StringComparison.OrdinalIgnoreCase) != true)
        {
            return;
        }

        // MainWindow displays the actual refusal. Release this editor while preserving the draft,
        // so an asynchronous validation or save failure is easy to correct.
        if (ApsEdit.FailIfPending(ApsEdit.RequestVersion, Lang.T(
                "Ändringen bekräftades inte. Se meddelandet från motorn.",
                "The change was not confirmed. See the message from the engine.")))
        {
            RefreshApsControls();
        }
    }

    private async void OnCancelApsClientIdTest(object sender, RoutedEventArgs args)
    {
        if (ApsEdit.RequestId is not { } requestId || !_engine.IsConnected)
        {
            return;
        }

        var ack = await _engine.CancelApsClientIdTestAsync(requestId);
        ReportIfFailed(ack, Lang.T("Testet kunde inte avbrytas", "The test could not be cancelled"));

        // Only the correlated cancelled or saved event decides the outcome of a race.
        RefreshApsControls();
    }

    // ----------------------------------------------------- Interaxo client settings

    private void OnInteraxoFieldChanged(object sender, TextChangedEventArgs args)
    {
        if (!_mirroring)
        {
            _interaxoEdited = true;
        }
    }

    /// <summary>
    /// Sends the Interaxo client id and redirect URI, and the secret when one is typed. The secret box is
    /// emptied only once the engine has accepted it; a refused save keeps what was typed.
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
}
