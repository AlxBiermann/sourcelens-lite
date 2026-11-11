// Sets the theme before the first paint so dark-theme users see no white flash. Loaded as a blocking,
// external script because the Content-Security-Policy allows no inline scripts. Mirrors src/theme.ts.
(function () {
  var choice = null;
  try {
    choice = window.localStorage.getItem('sourcelens.theme');
  } catch (e) {
    // Storage unavailable: follow the system theme.
  }
  if (choice !== 'light' && choice !== 'dark') {
    choice = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }
  document.documentElement.dataset.theme = choice;
})();
