// Download combined video and audio using ffmpeg.wasm
// Usage: downloadCombinedVideo(videoUrl, audioUrl, downloadName)

(function() {
    const { FFmpeg } = FFmpegWASM;
    const { fetchFile } = FFmpegUtil;

    window.downloadCombinedVideo = async function(videoUrl, audioUrl, downloadName, onStatus) {
        const status = typeof onStatus === 'function' ? onStatus : function() {};
        const ffmpeg = new FFmpeg();

        try {
            status('loading');
            await ffmpeg.load({
                coreURL: '/static/ffmpeg/ffmpeg-core.js',
                wasmURL: '/static/ffmpeg/ffmpeg-core.wasm',
            });

            // 2. Fetch the separate files and write them to the virtual filesystem
            status('downloading');
            await ffmpeg.writeFile('input_video.mp4', await fetchFile(videoUrl));
            await ffmpeg.writeFile('input_audio.mp4', await fetchFile(audioUrl));

            // 3. Execute the muxing command (Copy codecs for speed)
            status('merging');
            await ffmpeg.exec([
                '-i', 'input_video.mp4',
                '-i', 'input_audio.mp4',
                '-c:v', 'copy',
                '-c:a', 'copy',
                '-map', '0:v:0',
                '-map', '1:a:0',
                'output.mp4'
            ]);

            // 4. Read the result and trigger a browser download
            const data = await ffmpeg.readFile('output.mp4');
            const url = URL.createObjectURL(new Blob([data.buffer], { type: 'video/mp4' }));

            const a = document.createElement('a');
            a.href = url;
            a.download = downloadName || 'combined_video.mp4';
            a.click();

            // Cleanup
            URL.revokeObjectURL(url);
            status('done');
        } catch (err) {
            status('error');
            throw err;
        }
    };

    // Human-readable label shown in the indicator for each stage
    const STATUS_LABELS = {
        loading: 'loading…',
        downloading: 'downloading…',
        merging: 'merging…',
        done: 'done ✓',
        error: 'failed ✗',
    };

    // Attach click handlers to combined download links
    function attachDownloadHandlers() {
        document.querySelectorAll('a.combined_download').forEach(function(link) {
            // Indicator span lives right after the link in the template
            const indicator = link.parentElement.querySelector('.download_status');

            function setStatus(stage) {
                if (!indicator) return;
                indicator.textContent = STATUS_LABELS[stage] || '';
                indicator.dataset.stage = stage;
                indicator.hidden = false;
            }

            link.addEventListener('click', function(e) {
                e.preventDefault();
                // Ignore extra clicks while a download is already in progress
                if (link.dataset.busy === 'true') return;
                link.dataset.busy = 'true';

                const videoUrl = link.getAttribute('data-video');
                const audioUrl = link.getAttribute('data-audio');
                const downloadName = link.getAttribute('data-name');

                downloadCombinedVideo(videoUrl, audioUrl, downloadName, setStatus)
                    .catch(function(err) {
                        console.error('Combined download failed:', err);
                    })
                    .finally(function() {
                        link.dataset.busy = 'false';
                    });
            });
        });
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', attachDownloadHandlers);
    } else {
        attachDownloadHandlers();
    }
})();
