using Synka.Services;
using Microsoft.UI.Xaml.Markup;

namespace Synka.Views;

/// <summary>
/// The language choice in XAML: <c>Text="{loc:T Sv='Synka', En='Sync'}"</c>.
/// Both languages stand on the line the text belongs on. The value is read once,
/// when the page is built; a language change rebuilds the page (see MainWindow).
/// </summary>
[MarkupExtensionReturnType(ReturnType = typeof(string))]
public sealed class T : MarkupExtension
{
    /// <summary>The Swedish text.</summary>
    public string Sv { get; set; } = string.Empty;

    /// <summary>The English text.</summary>
    public string En { get; set; } = string.Empty;

    protected override object ProvideValue() => Lang.T(Sv, En);
}
