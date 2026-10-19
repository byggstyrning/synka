using Microsoft.UI.Xaml;

namespace Synka.Views;

/// <summary>
/// A state trigger that answers to the width of the element it is declared on,
/// not the width of the window. The navigation pane takes 248 px of the window
/// when it is expanded and nothing when it is hidden, so a window breakpoint
/// would fold a page too early or too late.
///
/// Use: set <c>local:WidthTrigger.Track="True"</c> on the element that carries
/// the VisualStateGroups, and put a trigger with <see cref="Below"/> and/or
/// <see cref="AtLeast"/> in the states. The tracked element's SizeChanged drives
/// every WidthTrigger in its own groups.
///
/// Only the width is read, and the states change the height of what they fold,
/// never the width, so a state change cannot fire the trigger back with a
/// different answer.
/// </summary>
public sealed class WidthTrigger : StateTriggerBase
{
    /// <summary>Active while the tracked element is at least this wide, in DIPs.</summary>
    public double AtLeast { get; set; }

    /// <summary>Active while the tracked element is narrower than this, in DIPs.</summary>
    public double Below { get; set; } = double.PositiveInfinity;

    /// <summary>The attached switch that connects an element's SizeChanged to the WidthTriggers in its groups.</summary>
    public static readonly DependencyProperty TrackProperty = DependencyProperty.RegisterAttached(
        "Track", typeof(bool), typeof(WidthTrigger), new PropertyMetadata(false, OnTrackChanged));

    public static bool GetTrack(DependencyObject element) => (bool)element.GetValue(TrackProperty);

    public static void SetTrack(DependencyObject element, bool value) => element.SetValue(TrackProperty, value);

    private static void OnTrackChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args)
    {
        if (sender is not FrameworkElement element)
        {
            return;
        }

        if ((bool)args.NewValue)
        {
            element.SizeChanged += OnSizeChanged;
        }
        else
        {
            element.SizeChanged -= OnSizeChanged;
        }
    }

    private static void OnSizeChanged(object sender, SizeChangedEventArgs args)
    {
        if (sender is not FrameworkElement element)
        {
            return;
        }

        var width = args.NewSize.Width;
        foreach (var group in VisualStateManager.GetVisualStateGroups(element))
        {
            foreach (var state in group.States)
            {
                foreach (var trigger in state.StateTriggers)
                {
                    if (trigger is WidthTrigger widthTrigger)
                    {
                        widthTrigger.SetActive(widthTrigger.AtLeast <= width && width < widthTrigger.Below);
                    }
                }
            }
        }
    }
}
