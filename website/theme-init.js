try {
  const savedTheme = localStorage.getItem('tsz-theme');
  const prefersDark = matchMedia('(prefers-color-scheme: dark)').matches;
  document.documentElement.dataset.theme = savedTheme === 'light' || savedTheme === 'dark'
    ? savedTheme
    : prefersDark ? 'dark' : 'light';
} catch {
  document.documentElement.dataset.theme = 'light';
}
