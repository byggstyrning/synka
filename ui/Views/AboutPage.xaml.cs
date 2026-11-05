using System.ComponentModel;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Synka.Views;

public sealed partial class AboutPage : Page
{
    private readonly EngineClient _engine = EngineClient.Instance;

    private bool _attached;

    /// <summary>The descriptions of the default build, as the XAML has them.</summary>
    private readonly object _appDescription;
    private readonly object _privacyDescription;

    public AboutPage()
    {
        InitializeComponent();
        _appDescription = AppCard.Description;
        _privacyDescription = PrivacyCard.Description;

        VersionText.Text = Lang.T("version ", "version ") + EngineClient.AppVersion;
        ShowEngine();
        ShowPlatforms();

        // Attached while the page is in the window, and only then: the engine client
        // lives as long as the window and would otherwise keep every page alive.
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
        ShowEngine();
        ShowPlatforms();
    }

    private void OnUnloaded(object sender, RoutedEventArgs args)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        _engine.PropertyChanged -= OnEngineChanged;
    }

    private void OnEngineChanged(object? sender, PropertyChangedEventArgs args)
    {
        if (args.PropertyName is nameof(EngineClient.Status) or nameof(EngineClient.IsConnected))
        {
            ShowEngine();
        }
        else if (args.PropertyName is nameof(EngineClient.Settings))
        {
            ShowPlatforms();
        }
    }

    /// <summary>
    /// The ACC-only build (settings.build_providers) names Autodesk alone; every other build keeps the
    /// XAML text, which names all three platforms.
    /// </summary>
    private void ShowPlatforms()
    {
        var accOnly = !_engine.ProviderInBuild(ProviderNames.StreamBim)
            && !_engine.ProviderInBuild(ProviderNames.Interaxo);
        AppCard.Description = accOnly
            ? Lang.T(
                "Synkar dokument från Autodesk Construction Cloud (ACC) till Utforskaren. Kan köras bredvid Autodesk Desktop Connector.",
                "Syncs documents from Autodesk Construction Cloud (ACC) to Explorer. Can run alongside Autodesk Desktop Connector.")
            : _appDescription;
        PrivacyCard.Description = accOnly
            ? Lang.T(
                "Synka skickar dina filer och kontouppgifter bara mellan den här datorn och Autodesk (APS-tjänsterna, inloggningen och Autodesks fillagring). Ingen telemetri. Synka söker inte efter uppdateringar om inte din organisation har slagit på det.",
                "Synka sends your files and account data only between this PC and Autodesk (the APS endpoints, its sign-in and Autodesk file storage). No telemetry. Synka does not check for updates unless your organisation has turned that on.")
            : _privacyDescription;
    }

    /// <summary>The engine's version comes from its status. Until the engine answers, the page says so.</summary>
    private void ShowEngine()
    {
        var version = _engine.Status?.EngineVersion;
        EngineText.Text = !_engine.IsConnected
            ? Lang.T("ingen kontakt med motorn", "no contact with the engine")
            : version is not null
                ? Lang.T($"motor {version}", $"engine {version}")
                : Lang.T("ansluten, versionen hämtas …", "connected, fetching the version …");
    }
}
