function run(argv) {
    var target = argv[0];
    var names = ['Google Chrome', 'Safari'];
    for (var n = 0; n < names.length; n++) {
        var app;
        try { app = Application(names[n]); if (!app.running()) continue; } catch (e) { continue; }
        var windows = app.windows();
        for (var wi = 0; wi < windows.length; wi++) {
            var tabs = windows[wi].tabs();
            for (var ti = 0; ti < tabs.length; ti++) {
                var url = '';
                try { url = String(tabs[ti].url() || ''); } catch (e) { }
                if (url === target) {
                    if (names[n] === 'Safari') {
                        windows[wi].currentTab = tabs[ti];
                    } else {
                        windows[wi].activeTabIndex = ti + 1;
                    }
                    windows[wi].index = 1;
                    app.activate();
                    return JSON.stringify({ focused: true, browser: names[n] });
                }
            }
        }
    }
    throw Error('That tab is no longer open; run tabs to refresh');
}
