const comments = document.querySelectorAll('.comment');
for (const comment of comments) {
  comment.querySelector('.comment_collapse').addEventListener('click', () => {
    setTimeout(() => {
      comment.scrollIntoView({ 'block': 'nearest' });
    }, 50);
  })
}
