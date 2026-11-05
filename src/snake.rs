//! The snake game of the help overlay's hidden tab. Pure game rules; input, timing and drawing live in `ui`.

use std::collections::VecDeque;
use std::time::Duration;

pub const WIDTH: i16 = 20;
pub const HEIGHT: i16 = 12;

const START_INTERVAL_MS: u64 = 170;
const FASTEST_INTERVAL_MS: u64 = 70;
const SPEEDUP_MS_PER_FOOD: u64 = 6;
/// Turns accepted between two steps, so quick double turns are not lost.
const MAX_QUEUED_TURNS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn delta(self) -> (i16, i16) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }

    fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ready,
    Running,
    Paused,
    /// The snake hit a wall or itself.
    Crashed,
    /// The snake fills the whole field.
    Won,
}

#[derive(Debug)]
pub struct Snake {
    /// Head first.
    body: VecDeque<(i16, i16)>,
    dir: Dir,
    turns: VecDeque<Dir>,
    food: (i16, i16),
    phase: Phase,
    score: u32,
    best: u32,
    rng: u64,
}

impl Snake {
    pub fn new(seed: u64) -> Self {
        let mut game = Self {
            body: VecDeque::new(),
            dir: Dir::Right,
            turns: VecDeque::new(),
            food: (0, 0),
            phase: Phase::Ready,
            score: 0,
            best: 0,
            rng: seed | 1,
        };
        game.reset();
        game
    }

    /// Back to the start position; the best score of this session stays.
    pub fn reset(&mut self) {
        let (x, y) = (WIDTH / 2, HEIGHT / 2);
        self.body = VecDeque::from([(x, y), (x - 1, y), (x - 2, y)]);
        self.dir = Dir::Right;
        self.turns.clear();
        self.score = 0;
        self.phase = Phase::Ready;
        self.place_food();
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn score(&self) -> u32 {
        self.score
    }

    pub fn best(&self) -> u32 {
        self.best
    }

    pub fn food(&self) -> (i16, i16) {
        self.food
    }

    pub fn head(&self) -> (i16, i16) {
        self.body[0]
    }

    pub fn contains(&self, cell: (i16, i16)) -> bool {
        self.body.contains(&cell)
    }

    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// The time between two steps; it shrinks with every food eaten.
    pub fn interval(&self) -> Duration {
        let faster = u64::from(self.score) * SPEEDUP_MS_PER_FOOD;
        Duration::from_millis(
            START_INTERVAL_MS
                .saturating_sub(faster)
                .max(FASTEST_INTERVAL_MS),
        )
    }

    /// Enter / Space: starts, pauses, resumes or restarts.
    pub fn toggle(&mut self) {
        match self.phase {
            Phase::Ready | Phase::Paused => self.phase = Phase::Running,
            Phase::Running => self.phase = Phase::Paused,
            Phase::Crashed | Phase::Won => {
                self.reset();
                self.phase = Phase::Running;
            }
        }
    }

    /// An arrow key. The first arrow also starts a game that has not begun.
    pub fn turn(&mut self, dir: Dir) {
        match self.phase {
            Phase::Ready | Phase::Paused => self.phase = Phase::Running,
            Phase::Running => {}
            Phase::Crashed | Phase::Won => return,
        }
        let last = self.turns.back().copied().unwrap_or(self.dir);
        if dir != last && dir != last.opposite() && self.turns.len() < MAX_QUEUED_TURNS {
            self.turns.push_back(dir);
        }
    }

    /// Moves the snake one cell.
    pub fn step(&mut self) {
        if self.phase != Phase::Running {
            return;
        }
        if let Some(dir) = self.turns.pop_front() {
            self.dir = dir;
        }
        let (dx, dy) = self.dir.delta();
        let (x, y) = self.head();
        let next = (x + dx, y + dy);
        let grows = next == self.food;
        // The tail cell is free again unless the snake grows into it.
        let blocked = self
            .body
            .iter()
            .take(self.body.len() - usize::from(!grows))
            .any(|cell| *cell == next);
        if !(0..WIDTH).contains(&next.0) || !(0..HEIGHT).contains(&next.1) || blocked {
            self.phase = Phase::Crashed;
            return;
        }
        self.body.push_front(next);
        if grows {
            self.score += 1;
            self.best = self.best.max(self.score);
            if self.body.len() == (WIDTH * HEIGHT) as usize {
                self.phase = Phase::Won;
            } else {
                self.place_food();
            }
        } else {
            self.body.pop_back();
        }
    }

    fn place_food(&mut self) {
        let free: Vec<(i16, i16)> = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
            .filter(|cell| !self.body.contains(cell))
            .collect();
        if free.is_empty() {
            return;
        }
        self.food = free[self.random() as usize % free.len()];
    }

    /// xorshift64: enough for placing food.
    fn random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> Snake {
        let mut game = Snake::new(42);
        game.toggle();
        game
    }

    #[test]
    fn starts_ready_and_moves_only_while_running() {
        let mut game = Snake::new(1);
        let head = game.head();
        game.step();
        assert_eq!(game.phase(), Phase::Ready);
        assert_eq!(game.head(), head);
        game.toggle();
        game.step();
        assert_eq!(game.head(), (head.0 + 1, head.1));
        game.toggle();
        assert_eq!(game.phase(), Phase::Paused);
        game.step();
        assert_eq!(
            game.head(),
            (head.0 + 1, head.1),
            "paused snakes stand still"
        );
    }

    #[test]
    fn the_first_arrow_starts_and_reversing_is_ignored() {
        let mut game = Snake::new(1);
        game.turn(Dir::Left);
        assert!(game.is_running());
        game.step();
        assert_eq!(
            game.head(),
            (WIDTH / 2 + 1, HEIGHT / 2),
            "left would reverse into the neck"
        );
        game.turn(Dir::Up);
        game.step();
        assert_eq!(game.head(), (WIDTH / 2 + 1, HEIGHT / 2 - 1));
    }

    #[test]
    fn two_quick_turns_are_both_applied_and_a_third_is_dropped() {
        let mut game = running();
        let (x, y) = game.head();
        game.turn(Dir::Up);
        game.turn(Dir::Left);
        game.turn(Dir::Down);
        game.step();
        assert_eq!(game.head(), (x, y - 1));
        game.step();
        assert_eq!(game.head(), (x - 1, y - 1));
        game.step();
        assert_eq!(
            game.head(),
            (x - 2, y - 1),
            "down was dropped, the snake keeps going left"
        );
    }

    #[test]
    fn eating_grows_scores_and_speeds_up() {
        let mut game = running();
        let (x, y) = game.head();
        game.food = (x + 1, y);
        let before = game.interval();
        game.step();
        assert_eq!(game.score(), 1);
        assert_eq!(game.best(), 1);
        assert_eq!(game.body.len(), 4);
        assert!(game.interval() < before);
        assert!(
            !game.contains(game.food()),
            "new food is never inside the snake"
        );
    }

    #[test]
    fn the_interval_has_a_floor() {
        let mut game = running();
        game.score = 1000;
        assert_eq!(game.interval(), Duration::from_millis(FASTEST_INTERVAL_MS));
    }

    #[test]
    fn walls_end_the_game_and_enter_restarts_with_the_best_score_kept() {
        let mut game = running();
        game.best = 5;
        for _ in 0..WIDTH {
            game.step();
        }
        assert_eq!(game.phase(), Phase::Crashed);
        game.turn(Dir::Up);
        assert_eq!(game.phase(), Phase::Crashed, "arrows do not restart");
        game.toggle();
        assert!(game.is_running());
        assert_eq!((game.score(), game.best()), (0, 5));
    }

    #[test]
    fn biting_itself_ends_the_game_but_following_the_tail_does_not() {
        let mut game = running();
        game.body = VecDeque::from([(5, 5), (5, 6), (6, 6), (6, 5)]);
        game.dir = Dir::Up;
        game.food = (0, 0);
        game.turn(Dir::Right);
        game.step();
        assert!(game.is_running(), "the tail cell is free after the move");
        let mut game = running();
        game.body = VecDeque::from([(5, 5), (5, 6), (6, 6), (6, 5), (7, 5), (7, 4)]);
        game.dir = Dir::Up;
        game.food = (0, 0);
        game.turn(Dir::Right);
        game.step();
        assert_eq!(game.phase(), Phase::Crashed);
    }

    #[test]
    fn filling_the_field_wins() {
        let mut game = running();
        // A zig-zag path over all cells; the snake covers it except the last cell, which is the food.
        let path: Vec<(i16, i16)> = (0..HEIGHT)
            .flat_map(|y| {
                let row: Vec<i16> = if y % 2 == 0 {
                    (0..WIDTH).collect()
                } else {
                    (0..WIDTH).rev().collect()
                };
                row.into_iter().map(move |x| (x, y))
            })
            .collect();
        game.body = path[..path.len() - 1].iter().rev().copied().collect();
        game.food = path[path.len() - 1];
        game.dir = Dir::Left;
        game.step();
        assert_eq!(game.phase(), Phase::Won);
        assert_eq!(game.score(), 1);
    }
}
