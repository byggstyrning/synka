using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Collections.Specialized;
using System.ComponentModel;
using System.Linq;
using Synka.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace Synka.Views;

/// <summary>
/// One line of the activity list. It wraps an engine item and raises
/// PropertyChanged for RelativeTime, so the page's timer can re-read the
/// "för 3 min sedan" text without rebuilding the list.
/// </summary>
public sealed class ActivityRow : INotifyPropertyChanged
{
    public ActivityRow(ActivityItem item)
    {
        Item = item;
        FailureDetail = item.Kind is "error" or "unsupported" ? ErrorDisplay.FailureDetail(item.Detail) : string.Empty;
    }

    public ActivityItem Item { get; }

    public string Glyph => Fmt.KindGlyph(Item.Kind);

    public string KindLabel => Fmt.KindLabel(Item.Kind);

    /// <summary>The file (or "N filer"). An item without a name shows its kind instead of an empty line.</summary>
    public string Name => string.IsNullOrWhiteSpace(Item.Name) ? KindLabel : Item.Name;

    /// <summary>A multi-line detail is a file list: it goes in the hover, not on the row.</summary>
    private bool DetailIsList => Item.Detail?.Contains('\n') == true;

    public string Subtitle => Join(" · ", Item.Project, DetailIsList ? string.Empty : ErrorDisplay.FriendlyDetail(Item.Detail));

    /// <summary>The hover over a row: what, where, which files (+ new, ~ changed, - deleted), and when.</summary>
    public string RowToolTip => Join("\n", Name, Item.Project, DetailIsList ? Item.Detail! : FailureDetail.Length > 0 ? FailureDetail : ErrorDisplay.FriendlyDetail(Item.Detail), AbsoluteTime);

    /// <summary>The full safe reason belongs to the tooltip, not the compact visible row.</summary>
    public string FailureDetail { get; }

    public string FailureToolTip => FailureDetail.Length == 0 ? string.Empty : Join("\n", Name, Item.Project, FailureDetail);

    public string FileTypeLabel => Fmt.FileTypeLabel(Item.Name);

    public string FileTypeGlyph => Fmt.FileTypeGlyph(Item.Name);

    public string FileTypeDescription => Fmt.FileTypeDescription(Item.Name);

    public Visibility FileTypeVisibility => FileTypeLabel.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    public Visibility HelpVisibility =>
        ErrorDisplay.TryUnsupportedFileType(Item.Detail, out _) ? Visibility.Visible : Visibility.Collapsed;

    public string RelativeTime => Fmt.Ago(Item.TsMs);

    public string AbsoluteTime => Fmt.Absolute(Item.TsMs);

    public Visibility CriticalVisibility => Item.Kind == "error" ? Visibility.Visible : Visibility.Collapsed;

    public Visibility CautionVisibility => Item.Kind == "conflict" ? Visibility.Visible : Visibility.Collapsed;

    public Visibility NormalVisibility =>
        Item.Kind is "error" or "conflict" ? Visibility.Collapsed : Visibility.Visible;

    public event PropertyChangedEventHandler? PropertyChanged;

    public void RefreshTime()
    {
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(RelativeTime)));
    }

    /// <summary>
    /// What a screen reader says for the row. A list item that holds a data object
    /// takes its accessible name from the object's ToString(), which would otherwise
    /// be the class name.
    /// </summary>
    public override string ToString() => Join(", ", KindLabel, Item.Name, Item.Project, FailureDetail.Length > 0 ? FailureDetail : ErrorDisplay.FriendlyDetail(Item.Detail), RelativeTime);

    private static string Join(string separator, params string?[] parts) =>
        string.Join(separator, parts.Where(part => !string.IsNullOrWhiteSpace(part)));
}

public sealed partial class ActivityPage : Page
{
    private readonly ObservableCollection<ActivityRow> _view = new();

    // Rows by item INSTANCE, so a row keeps its identity (and its time text) while
    // the list is updated, and the ListView keeps its scroll position. By value
    // two identical events (same file, same millisecond) would share a row and
    // the same object would stand twice in the list.
    private Dictionary<ActivityItem, ActivityRow> _rows = new(ReferenceEqualityComparer.Instance);

    private DispatcherQueueTimer? _ticker;
    private bool _rebuildQueued;
    private bool _attached;
    private ToolTip? _focusToolTip;

    public ActivityPage()
    {
        InitializeComponent();
        EventList.ItemsSource = _view;
    }

    private static EngineClient Engine => EngineClient.Instance;

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        // Loaded can come twice for the same page; the handlers must be attached once.
        if (_attached)
        {
            return;
        }
        _attached = true;

        Engine.Activity.CollectionChanged += OnActivityChanged;
        Engine.PropertyChanged += OnEngineChanged;
        Rebuild();
        ShowQueue();

        // Relative times ("för 3 min sedan") age while the page is open.
        _ticker = DispatcherQueue.CreateTimer();
        _ticker.Interval = TimeSpan.FromSeconds(30);
        _ticker.Tick += OnTick;
        _ticker.Start();
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        if (!_attached)
        {
            return;
        }
        _attached = false;

        CloseFocusToolTip();
        Engine.Activity.CollectionChanged -= OnActivityChanged;
        Engine.PropertyChanged -= OnEngineChanged;
        if (_ticker is not null)
        {
            _ticker.Stop();
            _ticker.Tick -= OnTick;
            _ticker = null;
        }
    }

    private void OnActivityContainerChanging(ListViewBase sender, ContainerContentChangingEventArgs args)
    {
        var container = args.ItemContainer;
        if (ToolTipService.GetToolTip(container) is ToolTip oldToolTip)
        {
            oldToolTip.IsOpen = false;
            if (ReferenceEquals(oldToolTip, _focusToolTip)) _focusToolTip = null;
        }
        var detail = !args.InRecycleQueue && args.Item is ActivityRow row ? row.FailureToolTip : string.Empty;
        AutomationProperties.SetHelpText(container, detail);
        ToolTipService.SetToolTip(container, detail.Length == 0 ? null : new ToolTip
        {
            MaxWidth = 520,
            Content = new TextBlock { Text = detail, TextWrapping = TextWrapping.Wrap, MaxWidth = 480 },
        });
    }

    private void OnActivityGotFocus(object sender, RoutedEventArgs args)
    {
        if (XamlRoot is not { } root
            || FocusManager.GetFocusedElement(root) is not Control { FocusState: FocusState.Keyboard }) return;
        for (var element = args.OriginalSource as DependencyObject; element is not null && element != EventList;
             element = VisualTreeHelper.GetParent(element))
        {
            if (element is not ListViewItem container) continue;
            CloseFocusToolTip();
            if (ToolTipService.GetToolTip(container) is ToolTip toolTip)
            {
                _focusToolTip = toolTip;
                toolTip.IsOpen = true;
            }
            break;
        }
    }

    private void OnActivityLostFocus(object sender, RoutedEventArgs args) => CloseFocusToolTip();

    private void CloseFocusToolTip()
    {
        if (_focusToolTip is not null) _focusToolTip.IsOpen = false;
        _focusToolTip = null;
    }

    private void OnTick(DispatcherQueueTimer sender, object args)
    {
        foreach (var row in _rows.Values)
        {
            row.RefreshTime();
        }
    }

    private void OnEngineChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(EngineClient.Status) or nameof(EngineClient.IsConnected))
        {
            ShowQueue();
        }
    }

    private void OnActivityChanged(object? sender, NotifyCollectionChangedEventArgs e)
    {
        // A full list arrives as a reset plus up to 200 adds: rebuild once per UI turn, not per item.
        if (_rebuildQueued)
        {
            return;
        }

        _rebuildQueued = true;
        DispatcherQueue.TryEnqueue(() =>
        {
            _rebuildQueued = false;
            if (_attached)
            {
                Rebuild();
            }
        });
    }

    private void OnFilterChanged(object sender, SelectionChangedEventArgs e)
    {
        // The XAML sets the ComboBox's SelectedIndex, so this fires inside InitializeComponent,
        // before EmptyText (declared later) exists. OnLoaded builds the list afterwards anyway.
        if (EmptyText is null)
        {
            return;
        }
        Rebuild();
    }

    private async void OnOpenLog(object sender, RoutedEventArgs e)
    {
        var ack = await Engine.OpenLogAsync();
        if (!ack.Ok)
        {
            MainWindow.Instance?.ShowMessage(
                Lang.T("Loggmappen gick inte att öppna", "The log folder could not be opened"),
                ErrorDisplay.FriendlyDetail(ack.Error),
                InfoBarSeverity.Error);
        }
    }

    /// <summary>The failed jobs go back in the queue; the new counts come with the next `status`.</summary>
    private async void OnRetryFailed(object sender, RoutedEventArgs e)
    {
        var ack = await Engine.RetryFailedAsync();
        if (!ack.Ok)
        {
            MainWindow.Instance?.ShowMessage(
                Lang.T("Det gick inte att försöka igen", "The retry could not be started"),
                ErrorDisplay.FriendlyDetail(ack.Error),
                InfoBarSeverity.Error);
        }
    }

    private async void OnClearActivity(object sender, RoutedEventArgs e)
    {
        ClearActivityButton.IsEnabled = false;
        try
        {
            var ack = await Engine.ClearActivityAsync();
            if (!ack.Ok)
            {
                MainWindow.Instance?.ShowMessage(
                    Lang.T("Aktiviteten kunde inte rensas", "Activity could not be cleared"),
                    ErrorDisplay.FriendlyDetail(ack.Error),
                    InfoBarSeverity.Warning);
            }
        }
        finally
        {
            ClearActivityButton.IsEnabled = true;
        }
    }

    /// <summary>The engine's queue, beside the filter: what is waiting and what has failed.</summary>
    private void ShowQueue()
    {
        var status = Engine.Status;
        if (!Engine.IsConnected || status is null)
        {
            QueueText.Text = string.Empty;
            RetryButton.Visibility = Visibility.Collapsed;
            return;
        }

        RetryButton.Visibility = status.FailedJobs > 0 ? Visibility.Visible : Visibility.Collapsed;

        var parts = new List<string>();
        if (status.PendingJobs > 0)
        {
            var pending = Fmt.Count(status.PendingJobs);
            parts.Add(Lang.T($"{pending} i kö", $"{pending} queued"));
        }

        if (status.FailedJobs > 0)
        {
            var failed = Fmt.Count(status.FailedJobs);
            parts.Add(Lang.T($"{failed} misslyckade", $"{failed} failed"));
        }

        QueueText.Text = string.Join(" · ", parts);
    }

    private void Rebuild()
    {
        var filter = (FilterChoice.SelectedItem as ComboBoxItem)?.Tag as string ?? "all";

        var next = new Dictionary<ActivityItem, ActivityRow>(ReferenceEqualityComparer.Instance);
        var desired = new List<ActivityRow>();
        foreach (var item in Engine.Activity)
        {
            if (next.ContainsKey(item))
            {
                // The same instance twice cannot come from the engine client, but a
                // row may stand in the list only once.
                continue;
            }

            if (!_rows.TryGetValue(item, out var row))
            {
                row = new ActivityRow(item);
            }

            next[item] = row;
            if (Matches(item.Kind, filter))
            {
                desired.Add(row);
            }
        }

        _rows = next;
        ListSync.Apply(_view, desired);

        EmptyText.Text = filter switch
        {
            "problems" => Lang.T("Inga fel eller konflikter.", "No errors or conflicts."),
            "transfers" => Lang.T("Inga överföringar ännu.", "No transfers yet."),
            _ => Lang.T("Inga händelser här ännu.", "No events here yet."),
        };
        EmptyText.Visibility = _view.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        ClearActivityButton.IsEnabled = _view.Count > 0;
    }

    private static bool Matches(string? kind, string filter) => filter switch
    {
        "problems" => kind is "error" or "conflict" or "unsupported",
        "transfers" => kind is "download" or "upload" or "new_version",
        _ => true,
    };
}
