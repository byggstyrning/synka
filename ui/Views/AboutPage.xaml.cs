using System.ComponentModel;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Synka.Views;

public sealed partial class AboutPage : Page
{
    private readonly EngineClient _engine = EngineClient.Instance;

    private bool _attached;

    public AboutPage()
    {
        InitializeComponent();

        VersionText.Text = Lang.T("version ", "version ") + EngineClient.AppVersion;
        ShowEngine();

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
