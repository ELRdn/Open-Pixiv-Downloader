chrome.action.onClicked.addListener(async tab => {
  const url = chrome.runtime.getURL('report.html');
  const query = tab.id == null ? '' : '?tabId=' + encodeURIComponent(String(tab.id));
  await chrome.tabs.create({ url: url + query });
});
