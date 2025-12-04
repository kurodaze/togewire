function loadCacheStats() {
    fetch('/admin/cache/stats')
        .then(response => response.json())
        .then(data => {
            const sizeMB = data.size_mb ?? 0;
            const statsText = `${data.tracks} tracks cached (${sizeMB} MB)`;
            document.getElementById('cache-stats').textContent = statsText;
        })
        .catch(error => {
            console.error('Error fetching cache stats:', error);
            document.getElementById('cache-stats').textContent = 'Error loading cache stats';
        });
}

function clearCache() {
    if (confirm('Are you sure you want to clear the YouTube cache? This will remove all cached song matches.')) {
        fetch('/admin/cache/clear', { 
            method: 'POST'
        })
            .then(response => response.json())
            .then(data => {
                if (data.success) {
                    alert('Cache cleared successfully!');
                    loadCacheStats();
                } else {
                    alert('Error clearing cache: ' + (data.error || 'Unknown error'));
                }
            })
            .catch(error => {
                console.error('Error clearing cache:', error);
                alert('Error clearing cache: ' + error.message);
            });
    }
}

// Load cache stats on page load
document.addEventListener('DOMContentLoaded', loadCacheStats);
